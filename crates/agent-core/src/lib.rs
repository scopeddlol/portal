//! Home-side agent.
//!
//! The agent holds a WireGuard tunnel open to the gateway and bridges the
//! ports it is assigned to game servers on the local machine. It dials out and
//! is never dialled, which is the point: no port forward on the router, and
//! nothing at home is reachable from the internet except through the tunnel
//! the agent itself opened.
//!
//! - [`state`] — what it remembers between runs
//! - [`client`] — enrollment and assignment polling
//! - [`tunnel`] — bringing the tunnel up, behind a trait with two intended
//!   backends (kernel WireGuard, built; userspace boringtun, not yet)
//! - [`forward`] — the listeners that actually move traffic

pub mod client;
pub mod forward;
pub mod state;
pub mod tunnel;

use portal_proto::wg::generate_keypair;
use std::path::Path;
use std::time::Duration;

pub use client::{ClientError, GatewayClient};
pub use forward::Forwarder;
pub use state::{default_state_path, AgentState};
pub use tunnel::{ExistingTunnel, KernelTunnel, TunnelBackend};

/// How often the agent asks for its assignment.
///
/// Polling rather than a websocket: it is a handful of bytes every few
/// seconds, it recovers from a gateway restart without reconnect logic, and
/// the agent is behind home NAT where long-lived connections die quietly.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Enroll this machine with a gateway and write its state file.
///
/// The keypair is generated here and the private half never leaves: the
/// gateway is told only the public key, so a compromised gateway cannot
/// impersonate an agent to anything else.
pub async fn enroll(
    gateway_url: &str,
    token: &str,
    name: &str,
    state_path: &Path,
) -> anyhow::Result<AgentState> {
    let keys = generate_keypair();
    let client = GatewayClient::new(gateway_url, None)?;
    let response = client.enroll(token, name, keys.public.clone()).await?;

    let state = AgentState {
        gateway_url: gateway_url.trim_end_matches('/').to_string(),
        agent_id: response.agent_id,
        agent_key: response.agent_key,
        private_key: keys.private,
        tunnel: response.tunnel,
    };
    state.save(state_path)?;
    Ok(state)
}

/// Bring up the tunnel and serve the assignment until cancelled.
///
/// The assignment is applied declaratively on every poll, so a gateway that
/// was restarted, a network blip, or an update the agent slept through all
/// converge on the next tick without special handling.
pub async fn run(state: AgentState, backend: &dyn TunnelBackend) -> anyhow::Result<()> {
    backend.bring_up(&state.tunnel, &state.private_key)?;
    let bind_ip = backend.bind_address(&state.tunnel);
    tracing::info!(%bind_ip, gateway = %state.gateway_url, "tunnel up");

    let client = GatewayClient::new(&state.gateway_url, Some(state.agent_key.clone()))?;
    let mut forwarder = Forwarder::new();
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    let mut last_revision = None;

    loop {
        ticker.tick().await;
        match client.assignment().await {
            Ok(assignment) => {
                if last_revision == Some(assignment.revision) {
                    continue;
                }
                match forwarder.apply(bind_ip, &assignment.forwards).await {
                    Ok(()) => {
                        tracing::info!(
                            forwards = assignment.forwards.len(),
                            revision = assignment.revision,
                            "assignment applied"
                        );
                        last_revision = Some(assignment.revision);
                    }
                    // Leave the revision unset so the next tick retries rather
                    // than assuming this assignment is in place.
                    Err(e) => tracing::error!(error = %e, "could not apply the assignment"),
                }
            }
            Err(ClientError::Unauthorized) => {
                // Nothing to retry: the gateway has forgotten this agent, and
                // spinning forever would hide that from whoever is watching.
                forwarder.shutdown();
                anyhow::bail!("the gateway no longer recognises this agent; re-enroll it");
            }
            Err(e) => tracing::warn!(error = %e, "could not fetch the assignment; will retry"),
        }
    }
}
