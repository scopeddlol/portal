//! Tunnel subnet arithmetic.
//!
//! Every agent gets one address inside a small private subnet, and that
//! address is what nftables DNATs game traffic to. Handing out addresses is
//! therefore as load-bearing as handing out ports, and just as much worth
//! testing without a network.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

/// An IPv4 network in CIDR form, e.g. `10.99.0.0/24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Net {
    base: Ipv4Addr,
    prefix_len: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("`{0}` is not a CIDR network like 10.99.0.0/24")]
    Malformed(String),
    #[error("prefix length /{0} is out of range")]
    PrefixLen(u8),
    #[error("the tunnel subnet has no free addresses left")]
    Exhausted,
}

impl Ipv4Net {
    pub fn new(address: Ipv4Addr, prefix_len: u8) -> Result<Self, NetError> {
        if prefix_len > 32 {
            return Err(NetError::PrefixLen(prefix_len));
        }
        let mask = Self::mask_bits(prefix_len);
        Ok(Self {
            base: Ipv4Addr::from(u32::from(address) & mask),
            prefix_len,
        })
    }

    fn mask_bits(prefix_len: u8) -> u32 {
        if prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - prefix_len)
        }
    }

    pub fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    /// Network address, i.e. the `10.99.0.0` in `10.99.0.0/24`.
    pub fn base(self) -> Ipv4Addr {
        self.base
    }

    pub fn contains(self, addr: Ipv4Addr) -> bool {
        u32::from(addr) & Self::mask_bits(self.prefix_len) == u32::from(self.base)
    }

    /// Addresses that may be assigned to a host: everything except the network
    /// and broadcast addresses, which is what any other tool on the box will
    /// assume too.
    pub fn hosts(self) -> impl Iterator<Item = Ipv4Addr> {
        let first = u32::from(self.base).saturating_add(1);
        let last = u32::from(self.base) | !Self::mask_bits(self.prefix_len);
        let last = last.saturating_sub(1);
        (first..=last.max(first)).filter_map(move |n| {
            let addr = Ipv4Addr::from(n);
            (n <= last).then_some(addr)
        })
    }

    /// Lowest address in the subnet not already spoken for.
    ///
    /// Reuse is deliberate: an agent that is deleted frees its address for the
    /// next one, and the tunnel is sized for a household, not a datacenter.
    pub fn next_free(self, taken: &[Ipv4Addr]) -> Result<Ipv4Addr, NetError> {
        self.hosts()
            .find(|addr| !taken.contains(addr))
            .ok_or(NetError::Exhausted)
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.prefix_len)
    }
}

impl FromStr for Ipv4Net {
    type Err = NetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, len) = s
            .split_once('/')
            .ok_or_else(|| NetError::Malformed(s.to_string()))?;
        let addr: Ipv4Addr = addr
            .trim()
            .parse()
            .map_err(|_| NetError::Malformed(s.to_string()))?;
        let len: u8 = len
            .trim()
            .parse()
            .map_err(|_| NetError::Malformed(s.to_string()))?;
        Self::new(addr, len)
    }
}

impl<'de> serde::Deserialize<'de> for Ipv4Net {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for Ipv4Net {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_masks_to_the_network_address() {
        let n = net("10.99.0.37/24");
        assert_eq!(n.base(), Ipv4Addr::new(10, 99, 0, 0));
        assert_eq!(n.to_string(), "10.99.0.0/24");
    }

    #[test]
    fn rejects_junk() {
        assert!("10.99.0.0".parse::<Ipv4Net>().is_err());
        assert!("not/a/net".parse::<Ipv4Net>().is_err());
        assert!("10.99.0.0/33".parse::<Ipv4Net>().is_err());
    }

    #[test]
    fn hosts_exclude_network_and_broadcast() {
        let hosts: Vec<_> = net("10.99.0.0/29").hosts().collect();
        assert_eq!(hosts.first(), Some(&Ipv4Addr::new(10, 99, 0, 1)));
        assert_eq!(hosts.last(), Some(&Ipv4Addr::new(10, 99, 0, 6)));
        assert_eq!(hosts.len(), 6);
    }

    #[test]
    fn allocation_skips_taken_addresses() {
        let n = net("10.99.0.0/24");
        let taken = [Ipv4Addr::new(10, 99, 0, 1), Ipv4Addr::new(10, 99, 0, 2)];
        assert_eq!(n.next_free(&taken).unwrap(), Ipv4Addr::new(10, 99, 0, 3));
    }

    #[test]
    fn a_full_subnet_is_an_error_not_a_wrap_around() {
        let n = net("10.99.0.0/30");
        let taken: Vec<_> = n.hosts().collect();
        assert!(matches!(n.next_free(&taken), Err(NetError::Exhausted)));
    }

    #[test]
    fn containment_is_by_prefix() {
        let n = net("10.99.0.0/24");
        assert!(n.contains(Ipv4Addr::new(10, 99, 0, 1)));
        assert!(!n.contains(Ipv4Addr::new(10, 99, 1, 1)));
    }
}
