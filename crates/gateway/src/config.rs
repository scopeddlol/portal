//! Gateway configuration.
//!
//! Secrets are deliberately not part of the config file. The Cloudflare token
//! can edit DNS for a whole zone and the admin token is a login, so both are
//! read from the environment or from a file path named by the config — never
//! from the file that someone will eventually paste into a forum post while
//! asking why their ports do not work.

use crate::alloc::EdgePortRange;
use crate::net::Ipv4Net;
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("edge port range is invalid: {0}")]
    Range(#[from] crate::alloc::AllocError),
    #[error(
        "no {name}: set the `{env}` environment variable, or point `{key}` at a file containing it"
    )]
    MissingSecret {
        name: &'static str,
        env: &'static str,
        key: &'static str,
    },
    #[error("failed to read secret from `{path}`: {source}")]
    SecretFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the gateway's tunnel address {0} is not inside the tunnel subnet {1}")]
    GatewayIpOutsideSubnet(Ipv4Addr, Ipv4Net),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub gateway: GatewaySection,
    pub tunnel: TunnelSection,
    pub cloudflare: CloudflareSection,
    #[serde(default)]
    pub nftables: NftablesSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewaySection {
    /// Public IPv4 of the VPS. This is what goes in DNS and what players
    /// connect to; the home IP is never published.
    pub public_ip: Ipv4Addr,
    /// The Cloudflare zone being managed, e.g. `example.com`.
    pub zone: String,
    /// Where the HTTP API and web UI listen. Bind to localhost and put a
    /// reverse proxy in front unless you enjoy surprises.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_profiles_dir")]
    pub profiles_dir: PathBuf,
    #[serde(default = "default_port_range_start")]
    pub edge_port_range_start: u16,
    #[serde(default = "default_port_range_end")]
    pub edge_port_range_end: u16,
    /// Ports the gateway must never hand out, on top of whatever services
    /// already hold. SSH and the WireGuard endpoint are added automatically.
    #[serde(default)]
    pub reserved_tcp_ports: Vec<u16>,
    #[serde(default)]
    pub reserved_udp_ports: Vec<u16>,
    /// File holding the admin token. When absent, `PORTAL_ADMIN_TOKEN` is used,
    /// and failing that one is generated at startup and logged once.
    #[serde(default)]
    pub admin_token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TunnelSection {
    #[serde(default = "default_subnet")]
    pub subnet: Ipv4Net,
    #[serde(default = "default_gateway_ip")]
    pub gateway_ip: Ipv4Addr,
    #[serde(default = "default_wg_interface")]
    pub interface: String,
    #[serde(default = "default_wg_port")]
    pub listen_port: u16,
    /// `host:port` agents dial to reach WireGuard. Usually the VPS's public IP
    /// and `listen_port`, but a hostname works and survives an IP change.
    pub endpoint: String,
    /// Path to the gateway's WireGuard private key. Generated on first start
    /// if missing.
    #[serde(default = "default_key_file")]
    pub private_key_file: PathBuf,
    /// Agents are behind home NAT, so they must keep the binding alive; the
    /// gateway can never dial them first.
    #[serde(default = "default_keepalive")]
    pub persistent_keepalive: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudflareSection {
    pub zone_id: String,
    /// File holding the API token. `PORTAL_CF_API_TOKEN` takes precedence.
    #[serde(default)]
    pub api_token_file: Option<PathBuf>,
    /// Set false to run everything except the DNS writes, which is the honest
    /// way to try this out against a real zone.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NftablesSection {
    #[serde(default = "default_nft_table")]
    pub table: String,
    /// Set false on a box where something else owns the ruleset; the gateway
    /// will compute rules and log them without applying.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for NftablesSection {
    fn default() -> Self {
        Self {
            table: default_nft_table(),
            enabled: true,
        }
    }
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8080".parse().expect("literal")
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/portal")
}
fn default_profiles_dir() -> PathBuf {
    PathBuf::from("/etc/portal/profiles")
}
fn default_port_range_start() -> u16 {
    EdgePortRange::DEFAULT.start()
}
fn default_port_range_end() -> u16 {
    EdgePortRange::DEFAULT.end()
}
fn default_subnet() -> Ipv4Net {
    "10.99.0.0/24".parse().expect("literal")
}
fn default_gateway_ip() -> Ipv4Addr {
    Ipv4Addr::new(10, 99, 0, 1)
}
fn default_wg_interface() -> String {
    "wg0".to_string()
}
fn default_wg_port() -> u16 {
    51820
}
fn default_key_file() -> PathBuf {
    PathBuf::from("/etc/portal/wg-private.key")
}
fn default_keepalive() -> u16 {
    25
}
fn default_nft_table() -> String {
    "portal".to_string()
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let config: Config = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.edge_port_range()?;
        if !self.tunnel.subnet.contains(self.tunnel.gateway_ip) {
            return Err(ConfigError::GatewayIpOutsideSubnet(
                self.tunnel.gateway_ip,
                self.tunnel.subnet,
            ));
        }
        Ok(())
    }

    pub fn edge_port_range(&self) -> Result<EdgePortRange, ConfigError> {
        Ok(EdgePortRange::new(
            self.gateway.edge_port_range_start,
            self.gateway.edge_port_range_end,
        )?)
    }

    pub fn database_path(&self) -> PathBuf {
        self.gateway.data_dir.join("portal.db")
    }

    /// Ports that must stay clear of allocation: the gateway's own listener if
    /// it is on the public interface, the WireGuard endpoint, SSH, and
    /// anything the operator listed.
    pub fn reserved_ports(&self) -> Vec<(portal_proto::Protocol, u16)> {
        use portal_proto::Protocol::{Tcp, Udp};
        let mut reserved = vec![(Tcp, 22u16), (Udp, self.tunnel.listen_port)];
        reserved.push((Tcp, self.gateway.listen.port()));
        reserved.extend(self.gateway.reserved_tcp_ports.iter().map(|p| (Tcp, *p)));
        reserved.extend(self.gateway.reserved_udp_ports.iter().map(|p| (Udp, *p)));
        reserved
    }

    pub fn cloudflare_token(&self) -> Result<String, ConfigError> {
        read_secret(
            "PORTAL_CF_API_TOKEN",
            self.cloudflare.api_token_file.as_deref(),
        )?
        .ok_or(ConfigError::MissingSecret {
            name: "Cloudflare API token",
            env: "PORTAL_CF_API_TOKEN",
            key: "cloudflare.api_token_file",
        })
    }

    /// The admin token, or `None` when the operator has not set one and the
    /// caller should generate a temporary one.
    pub fn admin_token(&self) -> Result<Option<String>, ConfigError> {
        read_secret(
            "PORTAL_ADMIN_TOKEN",
            self.gateway.admin_token_file.as_deref(),
        )
    }
}

fn read_secret(env: &'static str, file: Option<&Path>) -> Result<Option<String>, ConfigError> {
    if let Ok(value) = std::env::var(env) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Ok(Some(value));
        }
    }
    let Some(path) = file else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::SecretFile {
        path: path.display().to_string(),
        source,
    })?;
    let raw = raw.trim().to_string();
    Ok((!raw.is_empty()).then_some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_proto::Protocol;

    const MINIMAL: &str = r#"
[gateway]
public_ip = "203.0.113.10"
zone = "example.com"

[tunnel]
endpoint = "vps.example.com:51820"

[cloudflare]
zone_id = "abc123"
"#;

    fn parse(toml_src: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(toml_src).map_err(|source| ConfigError::Parse {
            path: "test".into(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn a_minimal_config_gets_working_defaults() {
        let config = parse(MINIMAL).expect("minimal config should be enough");
        assert_eq!(config.gateway.listen.port(), 8080);
        assert_eq!(config.tunnel.subnet.to_string(), "10.99.0.0/24");
        assert_eq!(config.tunnel.gateway_ip, Ipv4Addr::new(10, 99, 0, 1));
        assert_eq!(config.tunnel.persistent_keepalive, 25);
        assert!(config.cloudflare.enabled);
        assert_eq!(config.nftables.table, "portal");
    }

    #[test]
    fn wireguard_and_ssh_ports_are_reserved_without_being_asked() {
        let reserved = parse(MINIMAL).unwrap().reserved_ports();
        assert!(reserved.contains(&(Protocol::Udp, 51820)));
        assert!(reserved.contains(&(Protocol::Tcp, 22)));
        assert!(reserved.contains(&(Protocol::Tcp, 8080)));
    }

    #[test]
    fn a_gateway_ip_outside_its_own_subnet_is_rejected() {
        let src = format!("{MINIMAL}\n[tunnel.extra]\n").replace(
            "endpoint = \"vps.example.com:51820\"",
            "endpoint = \"vps.example.com:51820\"\ngateway_ip = \"192.168.1.1\"",
        );
        let src = src.replace("\n[tunnel.extra]\n", "");
        assert!(matches!(
            parse(&src),
            Err(ConfigError::GatewayIpOutsideSubnet(..))
        ));
    }

    #[test]
    fn an_inverted_port_range_is_rejected_at_load() {
        let src = MINIMAL.replace(
            "zone = \"example.com\"",
            "zone = \"example.com\"\nedge_port_range_start = 40000\nedge_port_range_end = 30000",
        );
        assert!(matches!(parse(&src), Err(ConfigError::Range(_))));
    }

    #[test]
    fn secrets_come_from_the_environment_before_any_file() {
        // Safety: single-threaded within this test's scope and the variable is
        // read back immediately; no other test touches this name.
        unsafe { std::env::set_var("PORTAL_TEST_SECRET", "from-env") };
        let value = read_secret("PORTAL_TEST_SECRET", Some(Path::new("/nonexistent"))).unwrap();
        assert_eq!(value.as_deref(), Some("from-env"));
        unsafe { std::env::remove_var("PORTAL_TEST_SECRET") };
    }

    #[test]
    fn a_missing_cloudflare_token_names_both_ways_to_set_it() {
        let config = parse(MINIMAL).unwrap();
        let err = config.cloudflare_token().expect_err("no token configured");
        let msg = err.to_string();
        assert!(msg.contains("PORTAL_CF_API_TOKEN"), "{msg}");
        assert!(msg.contains("cloudflare.api_token_file"), "{msg}");
    }
}
