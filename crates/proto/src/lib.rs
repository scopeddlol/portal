//! Types shared between the VPS gateway and the home-side agent.
//!
//! Nothing in here talks to the network or the database; it is the vocabulary
//! both halves of the system agree on, plus the game-profile schema that turns
//! "I run Minecraft with voice chat" into a concrete set of port mappings and
//! DNS records.

pub mod api;
pub mod model;
pub mod profile;
pub mod wg;

pub use model::{Agent, Endpoint, PortMapping, Protocol, Service};
pub use profile::{Profile, ProfileSet, PortTemplate, SrvSpec};
