//! Core domain model.
//!
//! Three things, and the relationship between them is the whole product:
//!
//! - A **node** is a machine at home running the agent. It holds one end of a
//!   WireGuard tunnel and can reach everything on its LAN.
//! - A **service** is one subdomain, pointed at one node.
//! - A **port mapping** says "this public port reaches this address and port
//!   behind that node".
//!
//! Because a mapping names its own `local_host`, one agent can serve any
//! number of machines on the network it sits in — ten Minecraft servers on ten
//! different LAN addresses need one agent, not ten.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::Ipv4Addr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    /// The token nftables and `wg` expect.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Protocol {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "tcp" => Ok(Protocol::Tcp),
            "udp" => Ok(Protocol::Udp),
            other => Err(format!("`{other}` is not tcp or udp")),
        }
    }
}

/// A machine running the agent, paired to the gateway over WireGuard.
///
/// Created in the web UI before the agent ever starts: the gateway assigns the
/// tunnel address up front so it stays put across agent restarts, and hands
/// back a key the agent authenticates with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: Uuid,
    /// Human label shown in the UI, e.g. "basement-box".
    pub name: String,
    /// WireGuard public key, base64. `None` until the agent first registers;
    /// it changes whenever the agent restarts, which is expected and fine.
    pub public_key: Option<String>,
    /// Address inside the tunnel subnet, and the DNAT target. Stable for the
    /// life of the node.
    pub tunnel_ip: Ipv4Addr,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_handshake: Option<time::OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

impl Node {
    /// Nodes are online if WireGuard has completed a handshake recently.
    /// WireGuard rekeys every ~2 minutes when traffic flows and
    /// persistent-keepalive holds the NAT binding open, so a threshold a bit
    /// above that avoids flapping on idle tunnels.
    pub const ONLINE_THRESHOLD: time::Duration = time::Duration::seconds(180);

    pub fn is_online(&self, now: time::OffsetDateTime) -> bool {
        match self.last_handshake {
            Some(hs) => now - hs < Self::ONLINE_THRESHOLD,
            None => false,
        }
    }
}

/// One subdomain, served by one node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: Uuid,
    pub node_id: Uuid,
    /// Label for the UI, e.g. "SMP season 4".
    pub name: String,
    /// Left-hand label only: `mc` in `mc.example.com`.
    pub subdomain: String,
    pub enabled: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

impl Service {
    /// Fully qualified name, given the zone the gateway manages.
    pub fn fqdn(&self, zone: &str) -> String {
        if self.subdomain == "@" {
            zone.to_string()
        } else {
            format!("{}.{}", self.subdomain, zone)
        }
    }
}

/// One public port, forwarded to one address and port behind the node.
///
/// `edge_port` need not equal `local_port`: two servers can both want 25565
/// and only one can have it on the public IP. `local_host` is any address the
/// node can reach, which is what lets a single agent front a whole LAN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    pub id: Uuid,
    pub service_id: Uuid,
    pub protocol: Protocol,
    /// Where the server actually listens, as seen from the node.
    /// `192.168.1.50`, or `127.0.0.1` when it is on the node itself.
    pub local_host: String,
    pub local_port: u16,
    /// Port players connect to on the gateway's public IP.
    pub edge_port: u16,
    /// When set, an SRV record is published so compatible clients connect to
    /// the bare hostname with no port. This is what keeps ten Minecraft
    /// servers all reachable as plain names despite only one of them being
    /// able to hold 25565.
    pub srv: Option<SrvSpec>,
}

/// An SRV record published alongside a service's A record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SrvSpec {
    /// Leading-underscore service label, e.g. `_minecraft`.
    pub service: String,
    /// Leading-underscore protocol label, e.g. `_tcp`.
    pub proto: String,
    #[serde(default)]
    pub priority: u16,
    #[serde(default)]
    pub weight: u16,
}

impl SrvSpec {
    /// What Minecraft Java clients look up. The one case common enough to be
    /// worth a constructor.
    pub fn minecraft_java() -> Self {
        Self {
            service: "_minecraft".into(),
            proto: "_tcp".into(),
            priority: 0,
            weight: 5,
        }
    }

    /// The record name under a service's FQDN, e.g.
    /// `_minecraft._tcp.mc.example.com`.
    pub fn record_name(&self, service_fqdn: &str) -> String {
        format!("{}.{}.{}", self.service, self.proto, service_fqdn)
    }
}

/// What a player actually types, derived for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
    /// True when an SRV record makes the port implicit for compatible clients.
    pub port_implied_by_srv: bool,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.port_implied_by_srv {
            f.write_str(&self.host)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srv_record_name_is_fully_qualified() {
        assert_eq!(
            SrvSpec::minecraft_java().record_name("mc.example.com"),
            "_minecraft._tcp.mc.example.com"
        );
    }

    #[test]
    fn an_endpoint_hides_its_port_only_when_srv_covers_it() {
        let mut endpoint = Endpoint {
            host: "mc.example.com".into(),
            port: 30001,
            protocol: Protocol::Tcp,
            port_implied_by_srv: true,
        };
        assert_eq!(endpoint.to_string(), "mc.example.com");
        endpoint.port_implied_by_srv = false;
        assert_eq!(endpoint.to_string(), "mc.example.com:30001");
    }

    #[test]
    fn the_apex_service_is_the_zone_itself() {
        let service = Service {
            id: Uuid::nil(),
            node_id: Uuid::nil(),
            name: "root".into(),
            subdomain: "@".into(),
            enabled: true,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(service.fqdn("example.com"), "example.com");
    }

    #[test]
    fn protocols_round_trip_through_text() {
        for p in [Protocol::Tcp, Protocol::Udp] {
            assert_eq!(p.as_str().parse::<Protocol>().unwrap(), p);
        }
        assert!("sctp".parse::<Protocol>().is_err());
    }
}
