//! Request/response bodies for the gateway's HTTP API.
//!
//! Two callers with different credentials. A person holds the admin token and
//! drives everything. An agent holds its node key, and can do exactly two
//! things: register its tunnel identity, and read its own forwards.

use crate::model::{Endpoint, Node, PortMapping, Protocol, Service};
use crate::wg::PublicKey;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use uuid::Uuid;

// ---- nodes --------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNodeRequest {
    pub name: String,
}

/// The node, plus the key its agent authenticates with.
///
/// The key is shown once and stored only as a hash. It goes straight into the
/// agent's compose file — that is the entire setup on the home side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNodeResponse {
    #[serde(flatten)]
    pub node: Node,
    pub key: String,
}

/// Agent -> gateway on every start, authenticated with the node key.
///
/// The agent generates a fresh WireGuard keypair each time it boots and tells
/// the gateway the public half, so it needs to persist nothing at all. The
/// tunnel address is fixed by the gateway when the node is created, so
/// restarts do not disturb the forwards pointing at it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub public_key: PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub node_id: Uuid,
    pub tunnel: TunnelConfig,
}

/// Everything the agent needs to bring up its side of the tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub gateway_public_key: PublicKey,
    /// `host:port` the agent sends WireGuard traffic to.
    pub gateway_endpoint: String,
    /// Address assigned to this node inside the tunnel.
    pub tunnel_ip: Ipv4Addr,
    pub tunnel_prefix_len: u8,
    /// Seconds between keepalives. Non-zero so the agent's NAT binding stays
    /// open without requiring a port forward at home.
    pub persistent_keepalive: u16,
}

// ---- assignments --------------------------------------------------------

/// Gateway -> agent: the full set of forwards this node should serve.
///
/// Sent in full rather than as deltas; the agent applies it declaratively, so
/// a missed update self-heals on the next poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub revision: u64,
    pub forwards: Vec<Forward>,
}

/// One listener the agent accepts on its tunnel address and bridges to a
/// server on its network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forward {
    pub protocol: Protocol,
    /// Port the agent accepts on, inside the tunnel. Matches the DNAT target.
    pub tunnel_port: u16,
    /// Where the server is, as seen from the node. Any LAN address it can
    /// reach, which is what lets one agent front many machines.
    pub local_host: String,
    pub local_port: u16,
}

// ---- services and ports -------------------------------------------------

/// Step one: a subdomain on a node. Ports come afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateServiceRequest {
    pub node_id: Uuid,
    pub name: String,
    pub subdomain: String,
}

/// Step two: where the traffic actually goes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPortRequest {
    pub protocol: Protocol,
    /// LAN address of the server, e.g. `192.168.1.50`.
    pub local_host: String,
    pub local_port: u16,
    /// Public port. Left empty, the gateway picks — the local port when it is
    /// free, otherwise one from its range.
    #[serde(default)]
    pub edge_port: Option<u16>,
    /// Publish an SRV record so Minecraft Java clients need no port number.
    #[serde(default)]
    pub minecraft_srv: bool,
}

/// UI-facing view of a service with everything needed to render it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceView {
    #[serde(flatten)]
    pub service: Service,
    pub fqdn: String,
    pub node_name: String,
    pub node_online: bool,
    pub ports: Vec<PortView>,
    pub dns_synced: bool,
}

/// One mapping plus the address a player would type for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortView {
    #[serde(flatten)]
    pub mapping: PortMapping,
    pub endpoint: Endpoint,
    /// Pre-rendered, because this is the one string most people came for.
    pub connect: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}
