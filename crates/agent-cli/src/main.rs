//! Headless portal agent, for Linux and Docker.

use clap::{Parser, Subcommand};
use portal_agent_core::{
    default_state_path, enroll, run, AgentState, ExistingTunnel, KernelTunnel,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "portal-agent",
    version,
    about = "Publish local game servers through a portal gateway"
)]
struct Cli {
    /// Where the agent's credentials and tunnel settings live.
    #[arg(long, global = true)]
    state: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register this machine with a gateway, using a token from its web UI.
    Enroll {
        /// Base URL of the gateway, e.g. https://portal.example.com
        #[arg(long)]
        gateway: String,
        /// One-time enrollment token. Valid for an hour.
        #[arg(long)]
        token: String,
        /// Label shown in the web UI, e.g. basement-box.
        #[arg(long)]
        name: String,
    },
    /// Hold the tunnel open and serve whatever the gateway assigns.
    Run {
        /// Name of the WireGuard interface to manage.
        #[arg(long, default_value = "portal0")]
        interface: String,
        /// Directory for the generated WireGuard config.
        #[arg(long, default_value = "/etc/wireguard")]
        wireguard_dir: PathBuf,
        /// Assume the tunnel is already up and managed elsewhere. Useful when
        /// WireGuard is run by the host, systemd, or another container.
        #[arg(long)]
        no_tunnel: bool,
    },
    /// Print what this agent is and where it points, without secrets.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "portal_agent_core=info,portal_agent=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let state_path = cli.state.unwrap_or_else(default_state_path);

    match cli.command {
        Command::Enroll {
            gateway,
            token,
            name,
        } => {
            let state = enroll(&gateway, &token, &name, &state_path).await?;
            println!("Enrolled as {}", state.agent_id);
            println!("Tunnel address: {}", state.tunnel.tunnel_ip);
            println!("State written to {}", state_path.display());
            println!("\nStart forwarding with:\n  portal-agent run");
        }
        Command::Run {
            interface,
            wireguard_dir,
            no_tunnel,
        } => {
            let state = AgentState::load(&state_path)?;
            if no_tunnel {
                run(state, &ExistingTunnel).await?;
            } else {
                run(state, &KernelTunnel::new(interface, wireguard_dir)).await?;
            }
        }
        Command::Status => {
            let state = AgentState::load(&state_path)?;
            println!("Agent:    {}", state.agent_id);
            println!("Gateway:  {}", state.gateway_url);
            println!("Endpoint: {}", state.tunnel.gateway_endpoint);
            println!(
                "Tunnel:   {}/{}",
                state.tunnel.tunnel_ip, state.tunnel.tunnel_prefix_len
            );
        }
    }
    Ok(())
}
