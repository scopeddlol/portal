//! Edge port allocation.
//!
//! Two game servers on one machine can both want TCP 25565, and only one of
//! them can have it on the VPS's public IP. So the public port is allocated,
//! not assumed.
//!
//! Allocation prefers the game's well-known port whenever it is still free —
//! `mc.example.com` with nothing after it is worth something, and it keeps the
//! common single-server case looking exactly like a server with a forwarded
//! port. Only when that port is already spoken for does the allocator fall
//! back to a high range.

use portal_proto::Protocol;
use std::collections::HashSet;

/// Inclusive range the gateway draws from once a preferred port is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgePortRange {
    start: u16,
    end: u16,
}

impl EdgePortRange {
    /// Sits below Linux's default ephemeral range (32768-60999), so a port
    /// handed out here can never collide with the source port of an outbound
    /// connection the VPS makes on its own.
    pub const DEFAULT: Self = Self {
        start: 30000,
        end: 32767,
    };

    pub fn new(start: u16, end: u16) -> Result<Self, AllocError> {
        if start < 1024 || start > end {
            return Err(AllocError::InvalidRange(start, end));
        }
        Ok(Self { start, end })
    }

    pub fn start(self) -> u16 {
        self.start
    }

    pub fn end(self) -> u16 {
        self.end
    }

    pub fn contains(self, port: u16) -> bool {
        (self.start..=self.end).contains(&port)
    }
}

impl Default for EdgePortRange {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AllocError {
    #[error("edge port range {0}-{1} is invalid: start must be at least 1024 and not above end")]
    InvalidRange(u16, u16),
    #[error(
        "{protocol} port {port} cannot be moved (clients of this game cannot be told a different \
         port) and is already in use"
    )]
    FixedPortTaken { protocol: Protocol, port: u16 },
    #[error("no free {protocol} port left in the edge range {start}-{end}")]
    RangeExhausted {
        protocol: Protocol,
        start: u16,
        end: u16,
    },
}

/// What one port template needs from the public edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRequest {
    pub protocol: Protocol,
    /// The game's well-known port, used as-is when nothing else holds it.
    pub preferred: u16,
    /// Set when clients cannot be told about a different port, which turns
    /// "preferred" into "required": anything else is an error, not a fallback.
    pub fixed: bool,
}

impl PortRequest {
    pub fn flexible(protocol: Protocol, preferred: u16) -> Self {
        Self {
            protocol,
            preferred,
            fixed: false,
        }
    }

    pub fn fixed(protocol: Protocol, port: u16) -> Self {
        Self {
            protocol,
            preferred: port,
            fixed: true,
        }
    }
}

/// Tracks which `(protocol, port)` pairs are spoken for on the public IP.
///
/// TCP and UDP are allocated independently, because they are independent on
/// the wire: a Minecraft server on TCP 25565 does not stop a voice add-on from
/// using UDP 25565.
#[derive(Debug, Clone, Default)]
pub struct PortAllocator {
    range: EdgePortRange,
    taken: HashSet<(Protocol, u16)>,
}

impl PortAllocator {
    pub fn new(range: EdgePortRange) -> Self {
        Self {
            range,
            taken: HashSet::new(),
        }
    }

    /// Seed the allocator with ports that are already in use — mappings loaded
    /// from the database at startup, plus whatever the VPS itself listens on
    /// (SSH, the gateway's own HTTP port, the WireGuard endpoint).
    pub fn with_taken(
        range: EdgePortRange,
        taken: impl IntoIterator<Item = (Protocol, u16)>,
    ) -> Self {
        Self {
            range,
            taken: taken.into_iter().collect(),
        }
    }

    pub fn range(&self) -> EdgePortRange {
        self.range
    }

    pub fn is_taken(&self, protocol: Protocol, port: u16) -> bool {
        self.taken.contains(&(protocol, port))
    }

    /// Mark a port as unavailable. Returns false if it was already taken.
    pub fn reserve(&mut self, protocol: Protocol, port: u16) -> bool {
        self.taken.insert((protocol, port))
    }

    /// Give a port back, for when a service is deleted or a partially planned
    /// one is rolled back.
    pub fn release(&mut self, protocol: Protocol, port: u16) {
        self.taken.remove(&(protocol, port));
    }

    pub fn allocate(&mut self, req: PortRequest) -> Result<u16, AllocError> {
        if self.reserve(req.protocol, req.preferred) {
            return Ok(req.preferred);
        }
        if req.fixed {
            return Err(AllocError::FixedPortTaken {
                protocol: req.protocol,
                port: req.preferred,
            });
        }
        // Lowest free port first: deterministic, and it keeps allocated ports
        // clustered so the nftables ruleset stays easy to read.
        for port in self.range.start..=self.range.end {
            if self.reserve(req.protocol, port) {
                return Ok(port);
            }
        }
        Err(AllocError::RangeExhausted {
            protocol: req.protocol,
            start: self.range.start,
            end: self.range.end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator() -> PortAllocator {
        PortAllocator::new(EdgePortRange::new(30000, 30003).unwrap())
    }

    #[test]
    fn prefers_the_well_known_port_when_it_is_free() {
        let mut alloc = allocator();
        let port = alloc
            .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
            .unwrap();
        assert_eq!(port, 25565, "the first Minecraft server should look normal");
    }

    #[test]
    fn second_service_falls_back_into_the_range() {
        let mut alloc = allocator();
        alloc
            .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
            .unwrap();
        let second = alloc
            .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
            .unwrap();
        assert_eq!(second, 30000);
        assert!(alloc.range().contains(second));
    }

    #[test]
    fn tcp_and_udp_are_allocated_independently() {
        let mut alloc = allocator();
        alloc
            .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
            .unwrap();
        let udp = alloc
            .allocate(PortRequest::flexible(Protocol::Udp, 25565))
            .unwrap();
        assert_eq!(udp, 25565);
    }

    #[test]
    fn a_fixed_port_fails_rather_than_moving() {
        let mut alloc = allocator();
        alloc
            .allocate(PortRequest::fixed(Protocol::Udp, 19132))
            .unwrap();
        let err = alloc
            .allocate(PortRequest::fixed(Protocol::Udp, 19132))
            .expect_err("Bedrock's port must not be silently relocated");
        assert!(matches!(
            err,
            AllocError::FixedPortTaken {
                protocol: Protocol::Udp,
                port: 19132
            }
        ));
    }

    #[test]
    fn exhausting_the_range_is_an_error_not_a_wrap_around() {
        let mut alloc = PortAllocator::new(EdgePortRange::new(30000, 30001).unwrap());
        for _ in 0..3 {
            alloc
                .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
                .unwrap();
        }
        let err = alloc
            .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
            .expect_err("range holds two ports plus the preferred one");
        assert!(matches!(err, AllocError::RangeExhausted { .. }));
    }

    #[test]
    fn seeded_ports_are_not_handed_out() {
        let mut alloc = PortAllocator::with_taken(
            EdgePortRange::new(30000, 30003).unwrap(),
            [(Protocol::Tcp, 25565), (Protocol::Tcp, 30000)],
        );
        let port = alloc
            .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
            .unwrap();
        assert_eq!(port, 30001);
    }

    #[test]
    fn released_ports_come_back() {
        let mut alloc = allocator();
        alloc
            .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
            .unwrap();
        alloc.release(Protocol::Tcp, 25565);
        assert_eq!(
            alloc
                .allocate(PortRequest::flexible(Protocol::Tcp, 25565))
                .unwrap(),
            25565
        );
    }

    #[test]
    fn rejects_privileged_or_inverted_ranges() {
        assert!(matches!(
            EdgePortRange::new(80, 30000),
            Err(AllocError::InvalidRange(80, 30000))
        ));
        assert!(matches!(
            EdgePortRange::new(32767, 30000),
            Err(AllocError::InvalidRange(32767, 30000))
        ));
    }
}
