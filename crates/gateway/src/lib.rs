//! VPS-side gateway.
//!
//! The gateway decides things; the kernel does the work. Publishing a port is
//! an nftables DNAT rule into the tunnel subnet, so no game traffic passes
//! through this process and restarting it does not drop a player's connection.
//!
//! What lives here so far is the decision-making layer, which is pure and
//! testable without a VPS:
//!
//! - [`alloc`] — which public port a service gets, given what is already taken
//! - [`plan`] — profiles plus a request turned into port mappings, endpoints,
//!   config actions and DNS records
//! - [`dns`] — the desired zone contents, and the diff to get Cloudflare there
//!
//! The parts that touch the machine (HTTP API, storage, WireGuard peers,
//! nftables, the Cloudflare client) build on top of these.

pub mod alloc;
pub mod dns;
pub mod plan;

pub use alloc::{AllocError, EdgePortRange, PortAllocator, PortRequest};
pub use dns::{DnsPlan, DnsRecord, ExistingRecord};
pub use plan::{PlanError, Planner, ServicePlan};
