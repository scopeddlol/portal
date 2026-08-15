//! Types shared between the VPS gateway and the home-side agent.
//!
//! Nothing here talks to the network or the database; it is the vocabulary
//! both halves of the system agree on.

pub mod api;
pub mod model;
pub mod wg;

pub use model::{Endpoint, Node, PortMapping, Protocol, Service, SrvSpec};
