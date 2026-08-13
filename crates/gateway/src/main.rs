//! Gateway entry point.

use portal_gateway::cloudflare::Cloudflare;
use portal_gateway::http::{self, AppState};
use portal_gateway::{config::Config, store::Store, token, wgctl};
use portal_proto::profile::ProfileSet;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

/// How often the gateway re-checks reality: WireGuard handshakes for the
/// online indicator, and any DNS that failed to publish. Frequent enough that
/// a transient Cloudflare outage heals on its own, rare enough to be invisible
/// in the API rate limit.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "portal_gateway=info,tower_http=warn".into()),
        )
        .init();

    let config_path =
        std::env::var("PORTAL_CONFIG").unwrap_or_else(|_| "/etc/portal/config.toml".to_string());
    let config = Arc::new(Config::load(&config_path)?);
    tracing::info!(config = %config_path, zone = %config.gateway.zone, "starting");

    let profiles = Arc::new(ProfileSet::load_dir(&config.gateway.profiles_dir)?);
    tracing::info!(
        profiles = profiles.len(),
        dir = %config.gateway.profiles_dir.display(),
        "loaded game profiles"
    );

    let store = Arc::new(Store::open(config.database_path())?);

    // An operator who set no token gets a working one rather than a locked
    // door; it is printed once and changes on restart, which is the right
    // trade for a first run.
    let admin_token = match config.admin_token()? {
        Some(token) => token,
        None => {
            let generated = token::generate();
            tracing::warn!(
                token = %generated,
                "no admin token configured; generated a temporary one (set PORTAL_ADMIN_TOKEN to keep it across restarts)"
            );
            generated
        }
    };

    let cloudflare = if config.cloudflare.enabled {
        let token = config.cloudflare_token()?;
        Some(Arc::new(Cloudflare::new(
            &config.cloudflare.zone_id,
            token,
        )?))
    } else {
        tracing::warn!("cloudflare disabled; DNS records will not be published");
        None
    };

    // Make sure the key exists before anything asks for the public half, so
    // the first enrollment does not race file creation.
    let private_key = wgctl::load_or_create_private_key(&config.tunnel.private_key_file)?;
    if let Ok(public) = portal_proto::wg::public_from_private(&private_key) {
        tracing::info!(public_key = %public, endpoint = %config.tunnel.endpoint, "gateway tunnel identity");
    }

    let state = AppState {
        store: store.clone(),
        profiles,
        config: config.clone(),
        admin_token: Arc::new(admin_token),
        cloudflare,
    };

    // Bring the kernel in line with the database before serving: the machine
    // may have rebooted, and rules do not survive that.
    http::reconcile_edge(&state).await;

    tokio::spawn(background_reconcile(state.clone()));

    let listener = tokio::net::TcpListener::bind(config.gateway.listen).await?;
    tracing::info!(listen = %config.gateway.listen, "web UI and API ready");
    axum::serve(listener, http::router(state)).await?;
    Ok(())
}

/// Periodic catch-up: handshake times for the online indicator, and retries
/// for DNS that did not publish.
async fn background_reconcile(state: AppState) {
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    loop {
        ticker.tick().await;

        match wgctl::show_dump(&state.config.tunnel.interface) {
            Ok(dump) => {
                let peers = wgctl::parse_dump(&dump);
                if let Ok(agents) = state.store.list_agents() {
                    for agent in agents {
                        let Some(peer) = peers.iter().find(|p| p.public_key == agent.public_key)
                        else {
                            continue;
                        };
                        if let Some(at) = peer.last_handshake {
                            let _ = state.store.record_handshake(agent.id, at);
                        }
                    }
                }
            }
            // Expected when WireGuard is not up yet, so this is not an error.
            Err(e) => tracing::debug!(error = %e, "could not read WireGuard state"),
        }

        if let Ok(services) = state.store.list_services() {
            for service in services {
                if !state.store.is_dns_synced(service.id).unwrap_or(false) {
                    http::sync_dns(&state, service.id).await;
                }
            }
        }

        let _ = OffsetDateTime::now_utc();
    }
}
