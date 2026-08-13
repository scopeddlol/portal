//! Bringing up the agent's side of the tunnel.
//!
//! Two backends are foreseen and only one is built. Behind the trait, the
//! Linux/Docker agent hands the work to kernel WireGuard, where it is
//! essentially free. The Windows build is meant to run `boringtun` and
//! `smoltcp` in userspace so it needs no TUN driver and no administrator
//! rights — that backend is **not implemented yet**, and the trait exists so
//! adding it does not disturb the forwarding engine, which only ever asked
//! for an address to bind on.

use portal_proto::api::TunnelConfig;
use std::fmt::Write as _;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whatever gets decrypted traffic to the forwarding engine.
pub trait TunnelBackend: Send + Sync {
    /// Establish the tunnel. Must be idempotent: the agent calls it on every
    /// start, including after a crash that left the interface up.
    fn bring_up(&self, config: &TunnelConfig, private_key: &str) -> io::Result<()>;

    fn tear_down(&self) -> io::Result<()>;

    /// Address the forwarder should bind its listeners on.
    fn bind_address(&self, config: &TunnelConfig) -> IpAddr {
        IpAddr::V4(config.tunnel_ip)
    }
}

/// Kernel WireGuard, driven through `wg-quick`.
pub struct KernelTunnel {
    /// `wg-quick` takes the config path and names the interface after the
    /// file, so the path is the whole state this backend needs.
    config_path: PathBuf,
}

impl KernelTunnel {
    pub fn new(interface: impl Into<String>, config_dir: impl AsRef<Path>) -> Self {
        let config_path = config_dir
            .as_ref()
            .join(format!("{}.conf", interface.into()));
        Self { config_path }
    }
}

impl TunnelBackend for KernelTunnel {
    fn bring_up(&self, config: &TunnelConfig, private_key: &str) -> io::Result<()> {
        let rendered = render_config(config, private_key);
        write_private(&self.config_path, &rendered)?;

        // `wg-quick up` fails if the interface is already there, which happens
        // every time the agent restarts without the machine rebooting. Take it
        // down first and ignore the error from a device that was not present.
        let _ = Command::new("wg-quick")
            .arg("down")
            .arg(&self.config_path)
            .output();

        let out = Command::new("wg-quick")
            .arg("up")
            .arg(&self.config_path)
            .output()?;
        if out.status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "wg-quick up failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }

    fn tear_down(&self) -> io::Result<()> {
        let out = Command::new("wg-quick")
            .arg("down")
            .arg(&self.config_path)
            .output()?;
        if out.status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "wg-quick down failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// A backend for machines where the tunnel is somebody else's problem —
/// an existing WireGuard interface, or a test.
pub struct ExistingTunnel;

impl TunnelBackend for ExistingTunnel {
    fn bring_up(&self, _: &TunnelConfig, _: &str) -> io::Result<()> {
        Ok(())
    }
    fn tear_down(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Render the agent's `wg-quick` configuration.
pub fn render_config(config: &TunnelConfig, private_key: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Managed by portal-agent. Edits here are overwritten."
    );
    let _ = writeln!(out, "[Interface]");
    let _ = writeln!(out, "PrivateKey = {private_key}");
    let _ = writeln!(
        out,
        "Address = {}/{}",
        config.tunnel_ip, config.tunnel_prefix_len
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "[Peer]");
    let _ = writeln!(out, "PublicKey = {}", config.gateway_public_key);
    let _ = writeln!(out, "Endpoint = {}", config.gateway_endpoint);
    // Only the tunnel subnet: routing everything through the VPS would send
    // the household's whole internet connection over it, which is not what
    // anyone asked for by publishing a game server.
    let _ = writeln!(
        out,
        "AllowedIPs = {}/{}",
        subnet_base(config),
        config.tunnel_prefix_len
    );
    // The gateway can never dial home through NAT, so the agent is
    // responsible for keeping the binding open from its side.
    let _ = writeln!(out, "PersistentKeepalive = {}", config.persistent_keepalive);
    out
}

fn subnet_base(config: &TunnelConfig) -> std::net::Ipv4Addr {
    let mask = if config.tunnel_prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - config.tunnel_prefix_len.min(32))
    };
    std::net::Ipv4Addr::from(u32::from(config.tunnel_ip) & mask)
}

fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_proto::wg::generate_keypair;
    use std::net::Ipv4Addr;

    fn config() -> TunnelConfig {
        TunnelConfig {
            gateway_public_key: generate_keypair().public,
            gateway_endpoint: "vps.example.com:51820".into(),
            tunnel_ip: Ipv4Addr::new(10, 99, 0, 2),
            tunnel_prefix_len: 24,
            persistent_keepalive: 25,
        }
    }

    #[test]
    fn the_agent_config_carries_its_own_address_and_the_gateway_peer() {
        let rendered = render_config(&config(), "PRIVATE");
        assert!(rendered.contains("PrivateKey = PRIVATE"));
        assert!(rendered.contains("Address = 10.99.0.2/24"));
        assert!(rendered.contains("Endpoint = vps.example.com:51820"));
    }

    #[test]
    fn only_the_tunnel_subnet_is_routed_over_the_vpn() {
        let rendered = render_config(&config(), "PRIVATE");
        assert!(rendered.contains("AllowedIPs = 10.99.0.0/24"));
        assert!(
            !rendered.contains("0.0.0.0/0"),
            "publishing a game server must not reroute the household's internet"
        );
    }

    #[test]
    fn keepalive_is_set_because_the_gateway_cannot_dial_home() {
        assert!(render_config(&config(), "PRIVATE").contains("PersistentKeepalive = 25"));
    }

    #[test]
    fn the_bind_address_is_the_agents_tunnel_address() {
        let config = config();
        assert_eq!(
            ExistingTunnel.bind_address(&config),
            IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2))
        );
    }

    #[test]
    fn subnet_is_derived_from_the_assigned_address() {
        let mut config = config();
        config.tunnel_ip = Ipv4Addr::new(10, 99, 3, 200);
        config.tunnel_prefix_len = 16;
        assert_eq!(subnet_base(&config), Ipv4Addr::new(10, 99, 0, 0));
    }
}
