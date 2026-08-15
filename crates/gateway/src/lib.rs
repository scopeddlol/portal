//! VPS-side gateway.
//!
//! The gateway decides things; the kernel does the work. Publishing a port is
//! an nftables DNAT rule into the tunnel subnet, so no game traffic passes
//! through this process and restarting it does not drop a player's connection.
//!
//! The decision-making layer is pure and testable without a VPS:
//!
//! - [`alloc`] — which public port a service gets, given what is already taken
//! - [`plan`] — a service and its mappings turned into endpoints and DNS records
//! - [`dns`] — the desired zone contents, and the diff to get Cloudflare there
//! - [`net`] — tunnel address allocation
//!
//! The rest of it touches the machine:
//!
//! - [`store`] — SQLite, and the source of truth everything else is derived from
//! - [`http`] — API and web UI
//! - [`nft`] / [`wgctl`] — the kernel-facing halves
//! - [`cloudflare`] — DNS writes
//! - [`config`] / [`token`] — settings and credentials

pub mod alloc;
pub mod cloudflare;
pub mod config;
pub mod dns;
pub mod http;
pub mod net;
pub mod nft;
pub mod plan;
pub mod store;
pub mod token;
pub mod wgctl;

pub use alloc::{AllocError, EdgePortRange, PortAllocator, PortRequest};
pub use config::Config;
pub use dns::{DnsPlan, DnsRecord, ExistingRecord};
pub use net::Ipv4Net;
pub use plan::{describe_service, PlanError, ServiceDescription};
pub use store::Store;
