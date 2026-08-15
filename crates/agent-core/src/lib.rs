//! Home-side agent.
//!
//! The agent dials out and is never dialled: no port forward on the router,
//! and nothing at home is reachable from the internet except through the
//! tunnel the agent itself opened.
//!
//! It keeps **no state**. On every start it generates a fresh WireGuard
//! keypair, tells the gateway the public half, and gets back everything it
//! needs. That is why running it is setting two environment variables and
//! starting the container — there is no enrollment step, no state file to
//! preserve, and no volume that must survive.
//!
//! Because each forward names its own destination address, one agent fronts a
//! whole network: ten Minecraft servers on ten machines need one container.

pub mod client;
pub mod forward;
pub mod tunnel;

use portal_proto::wg::generate_keypair;
use std::time::Duration;

pub use client::{ClientError, GatewayClient};
pub use forward::Forwarder;
pub use tunnel::{ExistingTunnel, KernelTunnel, TunnelBackend};

/// How often the agent asks for its assignment.
///
/// Polling rather than a websocket: it is a handful of bytes every few
/// seconds, it recovers from a gateway restart without reconnect logic, and
/// the agent is behind home NAT where long-lived connections die quietly.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Longest wait between attempts to reach a gateway that is not answering.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Register with the gateway and serve the assignment until cancelled.
pub async fn run(gateway_url: &str, key: &str, backend: &dyn TunnelBackend) -> anyhow::Result<()> {
    let client = GatewayClient::new(gateway_url, key)?;

    // A fresh identity every boot. The node's tunnel address is fixed by the
    // gateway when the node is created, so nothing downstream cares that the
    // key changed.
    let keys = generate_keypair();
    let registration = register_with_retry(&client, &keys.public).await?;

    backend.bring_up(&registration.tunnel, &keys.private)?;
    let bind_ip = backend.bind_address(&registration.tunnel);
    tracing::info!(
        %bind_ip,
        gateway = client.base_url(),
        "tunnel up"
    );

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
                            "serving {} port(s)",
                            assignment.forwards.len()
                        );
                        last_revision = Some(assignment.revision);
                    }
                    // Leave the revision unset so the next tick retries rather
                    // than assuming this assignment is in place.
                    Err(e) => tracing::error!(error = %e, "could not apply the assignment"),
                }
            }
            Err(ClientError::Unauthorized) => {
                // Nothing to retry: the key is wrong or the node was deleted,
                // and spinning forever would hide that from whoever is
                // watching the logs.
                forwarder.shutdown();
                anyhow::bail!("the gateway does not recognise this node's key; check PORTAL_KEY");
            }
            Err(e) => tracing::warn!(error = %e, "could not fetch the assignment; will retry"),
        }
    }
}

/// Keep trying to register, backing off.
///
/// The gateway and the agent are usually started by different people at
/// different times, and a container that exits because the other end was not
/// up yet is a support question nobody should have to ask. A wrong key is the
/// one thing worth giving up on immediately.
async fn register_with_retry(
    client: &GatewayClient,
    public_key: &portal_proto::wg::PublicKey,
) -> anyhow::Result<portal_proto::api::RegisterResponse> {
    let mut wait = Duration::from_secs(2);
    loop {
        match client.register(public_key.clone()).await {
            Ok(response) => return Ok(response),
            Err(ClientError::Unauthorized) => {
                anyhow::bail!(
                    "the gateway does not recognise this node's key; copy PORTAL_KEY \
                     again from the control panel"
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "waiting for the gateway; retrying in {wait:?}");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(MAX_BACKOFF);
            }
        }
    }
}
