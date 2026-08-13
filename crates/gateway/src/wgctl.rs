//! Driving kernel WireGuard on the VPS.
//!
//! The gateway owns the peer list: every enrolled agent is a peer whose
//! allowed-ips is exactly its own tunnel address, so one compromised agent
//! cannot source traffic as another. Peers are synced wholesale, matching how
//! the rest of the gateway works — desired state computed, then applied.
//!
//! Agents are behind home NAT and the gateway can never dial them, so no peer
//! here has an endpoint. The agent connects out and the binding is held open
//! from its side by persistent-keepalive.

use portal_proto::model::Agent;
use std::fmt::Write as _;
use std::io;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;
use time::OffsetDateTime;

/// Interface-level settings for the gateway's own WireGuard device.
#[derive(Debug, Clone)]
pub struct InterfaceConfig {
    pub private_key: String,
    pub listen_port: u16,
    pub address: Ipv4Addr,
    pub prefix_len: u8,
}

/// Render a `wg-quick`-compatible configuration.
///
/// Written as a file rather than a series of `wg set` calls so that the
/// interface can be brought up by hand from the same content when something
/// has gone wrong and the gateway is not the thing you want in the loop.
pub fn render_config(iface: &InterfaceConfig, agents: &[Agent]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Managed by portal. Edits here are overwritten.");
    let _ = writeln!(out, "[Interface]");
    let _ = writeln!(out, "PrivateKey = {}", iface.private_key);
    let _ = writeln!(out, "ListenPort = {}", iface.listen_port);
    let _ = writeln!(out, "Address = {}/{}", iface.address, iface.prefix_len);
    for agent in agents {
        let _ = writeln!(out);
        let _ = writeln!(out, "# {}", agent.name);
        let _ = writeln!(out, "[Peer]");
        let _ = writeln!(out, "PublicKey = {}", agent.public_key);
        // A single address, not the subnet: this is the cryptokey routing
        // table, and a wider allowed-ips would let any agent impersonate any
        // other one.
        let _ = writeln!(out, "AllowedIPs = {}/32", agent.tunnel_ip);
    }
    out
}

/// One peer's liveness, from `wg show <iface> dump`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    pub public_key: String,
    /// `None` when the peer has never completed a handshake.
    pub last_handshake: Option<OffsetDateTime>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Parse `wg show <iface> dump`.
///
/// The first line describes the interface itself and is skipped; each
/// remaining line is a peer, tab separated:
/// `pubkey  psk  endpoint  allowed-ips  latest-handshake  rx  tx  keepalive`.
/// A handshake of `0` means "never", which is not the same as "at the epoch".
pub fn parse_dump(dump: &str) -> Vec<PeerStatus> {
    dump.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 7 {
                return None;
            }
            let handshake = cols[4].parse::<i64>().ok().filter(|s| *s > 0);
            Some(PeerStatus {
                public_key: cols[0].to_string(),
                last_handshake: handshake.and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok()),
                rx_bytes: cols[5].parse().unwrap_or(0),
                tx_bytes: cols[6].parse().unwrap_or(0),
            })
        })
        .collect()
}

/// Write the config and sync it onto a running interface.
///
/// `wg syncconf` applies the difference rather than tearing the interface
/// down, so adding an agent does not interrupt every other agent's tunnel.
pub fn apply_config(interface: &str, config_path: &Path, config: &str) -> io::Result<()> {
    write_private(config_path, config)?;
    let stripped = Command::new("wg-quick")
        .arg("strip")
        .arg(config_path)
        .output()?;
    if !stripped.status.success() {
        return Err(io::Error::other(format!(
            "wg-quick strip failed: {}",
            String::from_utf8_lossy(&stripped.stderr).trim()
        )));
    }
    let stripped_path = config_path.with_extension("stripped.conf");
    write_private(&stripped_path, &String::from_utf8_lossy(&stripped.stdout))?;
    let out = Command::new("wg")
        .arg("syncconf")
        .arg(interface)
        .arg(&stripped_path)
        .output()?;
    let _ = std::fs::remove_file(&stripped_path);
    if out.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "wg syncconf failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

pub fn show_dump(interface: &str) -> io::Result<String> {
    let out = Command::new("wg")
        .arg("show")
        .arg(interface)
        .arg("dump")
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(io::Error::other(format!(
            "wg show failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Read the gateway's private key, generating one on first run.
pub fn load_or_create_private_key(path: &Path) -> io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let pair = portal_proto::wg::generate_keypair();
    write_private(path, &format!("{}\n", pair.private))?;
    Ok(pair.private)
}

/// Write a file only the owner can read. A WireGuard private key in a
/// world-readable file is the whole tunnel handed over.
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
    use uuid::Uuid;

    fn agent(name: &str, key: &str, last_octet: u8) -> Agent {
        Agent {
            id: Uuid::new_v4(),
            name: name.to_string(),
            public_key: key.to_string(),
            tunnel_ip: Ipv4Addr::new(10, 99, 0, last_octet),
            last_handshake: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn iface() -> InterfaceConfig {
        InterfaceConfig {
            private_key: "PRIVATE".into(),
            listen_port: 51820,
            address: Ipv4Addr::new(10, 99, 0, 1),
            prefix_len: 24,
        }
    }

    #[test]
    fn every_agent_becomes_a_peer() {
        let config = render_config(
            &iface(),
            &[agent("basement", "KEY-A", 2), agent("attic", "KEY-B", 3)],
        );
        assert_eq!(config.matches("[Peer]").count(), 2);
        assert!(config.contains("PublicKey = KEY-A"));
        assert!(config.contains("PublicKey = KEY-B"));
    }

    #[test]
    fn a_peer_is_allowed_exactly_one_address() {
        let config = render_config(&iface(), &[agent("basement", "KEY-A", 2)]);
        assert!(
            config.contains("AllowedIPs = 10.99.0.2/32"),
            "a wider range would let one agent impersonate another"
        );
        assert!(!config.contains("10.99.0.0/24"));
    }

    #[test]
    fn peers_have_no_endpoint_because_the_gateway_never_dials_home() {
        let config = render_config(&iface(), &[agent("basement", "KEY-A", 2)]);
        assert!(!config.contains("Endpoint"));
    }

    #[test]
    fn the_interface_section_carries_key_port_and_address() {
        let config = render_config(&iface(), &[]);
        assert!(config.contains("PrivateKey = PRIVATE"));
        assert!(config.contains("ListenPort = 51820"));
        assert!(config.contains("Address = 10.99.0.1/24"));
    }

    #[test]
    fn dump_parsing_reads_handshakes_and_counters() {
        let dump = "PRIVKEY\tPUBKEY\t51820\toff\n\
                    KEY-A\t(none)\t203.0.113.5:1234\t10.99.0.2/32\t1700000000\t1024\t2048\t25\n";
        let peers = parse_dump(dump);
        assert_eq!(peers.len(), 1, "the interface line must not be a peer");
        assert_eq!(peers[0].public_key, "KEY-A");
        assert_eq!(peers[0].rx_bytes, 1024);
        assert_eq!(peers[0].tx_bytes, 2048);
        assert_eq!(
            peers[0].last_handshake.unwrap().unix_timestamp(),
            1700000000
        );
    }

    #[test]
    fn a_zero_handshake_means_never_not_1970() {
        let dump = "PRIVKEY\tPUBKEY\t51820\toff\n\
                    KEY-A\t(none)\t(none)\t10.99.0.2/32\t0\t0\t0\t25\n";
        let peers = parse_dump(dump);
        assert_eq!(peers[0].last_handshake, None);
    }

    #[test]
    fn malformed_dump_lines_are_skipped_not_panicked_on() {
        let dump =
            "iface\tline\there\tok\ngarbage\nKEY-A\t(none)\t(none)\t10.99.0.2/32\t0\t0\t0\t25\n";
        assert_eq!(parse_dump(dump).len(), 1);
    }

    #[test]
    fn a_generated_key_is_reused_on_the_next_start() {
        let dir = std::env::temp_dir().join(format!("portal-wg-{}", Uuid::new_v4()));
        let path = dir.join("wg.key");
        let first = load_or_create_private_key(&path).unwrap();
        let second = load_or_create_private_key(&path).unwrap();
        assert_eq!(
            first, second,
            "regenerating would orphan every enrolled agent"
        );
        assert!(portal_proto::wg::public_from_private(&first).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn key_files_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("portal-wg-{}", Uuid::new_v4()));
        let path = dir.join("wg.key");
        load_or_create_private_key(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
