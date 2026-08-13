//! Request/response bodies for the gateway's HTTP API.
//!
//! The agent uses `enroll` once, then polls (or holds a websocket for) its
//! assignment. Everything else is driven from the web UI.

use crate::model::{Endpoint, PortMapping, Protocol, Service};
use crate::wg::PublicKey;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use uuid::Uuid;

/// Agent -> gateway, exchanging a one-time enrollment token for a place in the
/// tunnel subnet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    pub name: String,
    pub public_key: PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub agent_id: Uuid,
    /// Long-lived credential for subsequent API calls.
    pub agent_key: String,
    pub tunnel: TunnelConfig,
}

/// Everything the agent needs to bring up its side of the tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub gateway_public_key: PublicKey,
    /// `host:port` the agent sends WireGuard traffic to.
    pub gateway_endpoint: String,
    /// Address assigned to this agent inside the tunnel.
    pub tunnel_ip: Ipv4Addr,
    pub tunnel_prefix_len: u8,
    /// Seconds between keepalives. Non-zero so the agent's NAT binding stays
    /// open without requiring a port forward at home.
    pub persistent_keepalive: u16,
}

/// Gateway -> agent: the full set of forwards this agent should serve.
///
/// Sent in full rather than as deltas; the agent applies it declaratively, so
/// a missed update self-heals on the next poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub revision: u64,
    pub forwards: Vec<Forward>,
}

/// One listener the agent should accept on its tunnel address and bridge to a
/// local game server port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forward {
    pub protocol: Protocol,
    /// Port the agent accepts on, inside the tunnel. Matches the DNAT target.
    pub tunnel_port: u16,
    /// Where the game server is actually listening on the agent's machine.
    pub local_host: String,
    pub local_port: u16,
}

/// UI-facing view of a service with everything needed to render it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceView {
    #[serde(flatten)]
    pub service: Service,
    pub fqdn: String,
    pub ports: Vec<PortMapping>,
    /// What players type, per port.
    pub endpoints: Vec<Endpoint>,
    /// Config keys the operator still needs to set on the game server.
    pub config_actions: Vec<ConfigAction>,
    pub dns_synced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigAction {
    pub file: String,
    pub key: String,
    pub value: String,
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateServiceRequest {
    pub agent_id: Uuid,
    pub name: String,
    pub subdomain: String,
    pub profiles: Vec<String>,
    /// Overrides for where the game server listens locally, keyed by
    /// `<profile-id>/<template-id>`. Anything omitted uses the profile default.
    #[serde(default)]
    pub local_port_overrides: std::collections::BTreeMap<String, u16>,
    /// Optional port templates the operator chose to enable, same key format.
    #[serde(default)]
    pub enabled_optional_ports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}
