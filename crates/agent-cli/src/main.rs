//! Headless portal agent.
//!
//! Configuration is two values, both accepted as environment variables so a
//! compose file is the whole setup:
//!
//!   PORTAL_URL=https://portal.example.com
//!   PORTAL_KEY=<the key shown when you created the node>

use clap::Parser;
use portal_agent_core::{run, ExistingTunnel, KernelTunnel};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "portal-agent",
    version,
    about = "Publish local servers through a portal gateway"
)]
struct Cli {
    /// Gateway address, e.g. https://portal.example.com
    #[arg(long, env = "PORTAL_URL")]
    url: String,

    /// Node key, shown once when you add the node in the control panel.
    #[arg(long, env = "PORTAL_KEY")]
    key: String,

    /// Name of the WireGuard interface to manage.
    #[arg(long, env = "PORTAL_INTERFACE", default_value = "portal0")]
    interface: String,

    /// Directory for the generated WireGuard config.
    #[arg(long, env = "PORTAL_WIREGUARD_DIR", default_value = "/etc/wireguard")]
    wireguard_dir: PathBuf,

    /// Assume the tunnel is already up and managed elsewhere.
    #[arg(
        long,
        env = "PORTAL_NO_TUNNEL",
        value_parser = lenient_bool,
        num_args = 0..=1,
        default_value = "false",
        default_missing_value = "true",
    )]
    no_tunnel: bool,
}

/// Accept the spellings people actually put in a compose file.
///
/// clap's own bool parser takes only `true`/`false`, and everybody writes `1`.
fn lenient_bool(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(format!("expected true or false, got `{other}`")),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "portal_agent_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!(gateway = %cli.url, "starting");

    if cli.no_tunnel {
        run(&cli.url, &cli.key, &ExistingTunnel).await
    } else {
        run(
            &cli.url,
            &cli.key,
            &KernelTunnel::new(cli.interface, cli.wireguard_dir),
        )
        .await
    }
}
