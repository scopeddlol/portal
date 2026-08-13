//! Core domain model.
//!
//! The shape that matters most here is `Service` -> many `PortMapping`. One
//! subdomain fronts an arbitrary number of ports across both protocols, which
//! is what lets a Minecraft server and its Simple Voice Chat UDP port live
//! behind a single hostname.

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

/// A machine running game servers, paired to the gateway over WireGuard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: Uuid,
    /// Human label shown in the web UI, e.g. "basement-box".
    pub name: String,
    /// WireGuard public key, base64. The private half never leaves the agent.
    pub public_key: String,
    /// Address assigned inside the tunnel subnet; the DNAT target.
    pub tunnel_ip: Ipv4Addr,
    pub last_handshake: Option<time::OffsetDateTime>,
    pub created_at: time::OffsetDateTime,
}

impl Agent {
    /// Agents are considered online if WireGuard has completed a handshake
    /// recently. WireGuard rekeys every ~2 minutes when traffic flows and
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

/// A named game server exposed on one subdomain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: Uuid,
    pub agent_id: Uuid,
    /// Label for the UI, e.g. "SMP season 4".
    pub name: String,
    /// Left-hand label only: `mc` in `mc.example.com`.
    pub subdomain: String,
    /// Profile ids contributing ports, e.g. `["minecraft-java", "simple-voice-chat"]`.
    pub profiles: Vec<String>,
    pub enabled: bool,
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

/// One port on the public edge, forwarded to one port on the game server.
///
/// `edge_port` is allocated by the gateway and is not necessarily equal to
/// `local_port` — two services on the same box can both want 25565, and only
/// one of them can have it on the public IP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    pub id: Uuid,
    pub service_id: Uuid,
    /// Which profile port template produced this mapping, for reconciliation
    /// and so the UI can explain what a port is for.
    pub template_id: String,
    pub protocol: Protocol,
    /// Port the game server listens on, behind the tunnel.
    pub local_port: u16,
    /// Port players connect to on the gateway's public IP.
    pub edge_port: u16,
}

/// What a player actually types, derived for display and for config hints.
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
