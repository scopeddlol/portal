//! The nftables ruleset that actually moves game traffic.
//!
//! This is the only place game packets are dealt with, and the gateway process
//! is not in the path: it writes rules, the kernel forwards. That is why a
//! gateway restart does not drop anyone's connection, and why the latency cost
//! of this whole system is the WireGuard encryption and nothing else.
//!
//! The ruleset is replaced wholesale rather than edited. Computing the desired
//! state and swapping it in atomically means the rules cannot drift away from
//! the database, and a crash half way through leaves the old ruleset intact.

use crate::store::ActiveForward;
use std::fmt::Write as _;
use std::io;
use std::process::{Command, Stdio};

/// Build the complete ruleset for the gateway's table.
///
/// The `table` / `delete table` / `table` preamble is the standard atomic
/// replace: the first line creates the table if it does not exist so the
/// delete cannot fail on a fresh boot, and because `nft -f` applies a file as
/// a single transaction, there is no window where the table is missing and
/// players are refused.
pub fn ruleset(table: &str, tunnel_subnet: &str, forwards: &[ActiveForward]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Managed by portal. Edits here are overwritten.");
    let _ = writeln!(out, "table ip {table}");
    let _ = writeln!(out, "delete table ip {table}");
    let _ = writeln!(out, "table ip {table} {{");
    let _ = writeln!(out, "  chain prerouting {{");
    let _ = writeln!(
        out,
        "    type nat hook prerouting priority dstnat; policy accept;"
    );
    for f in forwards {
        let _ = writeln!(
            out,
            "    {} dport {} dnat to {}:{}  # service {}",
            f.protocol, f.edge_port, f.tunnel_ip, f.edge_port, f.service_id
        );
    }
    let _ = writeln!(out, "  }}");
    let _ = writeln!(out, "  chain postrouting {{");
    let _ = writeln!(
        out,
        "    type nat hook postrouting priority srcnat; policy accept;"
    );
    // Without this the game server would reply straight to the player's real
    // address, which it has no route to; masquerading makes the agent talk
    // only to the gateway's tunnel address. The cost is that the server sees
    // the tunnel address instead of the player's IP.
    let _ = writeln!(out, "    ip daddr {tunnel_subnet} masquerade");
    let _ = writeln!(out, "  }}");
    let _ = writeln!(out, "}}");
    out
}

/// Hand a ruleset to `nft -f -`.
pub fn apply(ruleset: &str) -> io::Result<()> {
    use std::io::Write as _;
    let mut child = Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(ruleset.as_bytes())?;
    let out = child.wait_with_output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "nft exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_proto::Protocol;
    use std::net::Ipv4Addr;
    use uuid::Uuid;

    fn forward(protocol: Protocol, edge_port: u16, last_octet: u8) -> ActiveForward {
        ActiveForward {
            tunnel_ip: Ipv4Addr::new(10, 99, 0, last_octet),
            protocol,
            edge_port,
            local_port: 25565,
            service_id: Uuid::nil(),
        }
    }

    #[test]
    fn each_forward_becomes_one_dnat_rule() {
        let rules = ruleset(
            "portal",
            "10.99.0.0/24",
            &[
                forward(Protocol::Tcp, 25565, 2),
                forward(Protocol::Udp, 24454, 2),
            ],
        );
        assert!(rules.contains("tcp dport 25565 dnat to 10.99.0.2:25565"));
        assert!(rules.contains("udp dport 24454 dnat to 10.99.0.2:24454"));
    }

    #[test]
    fn the_table_is_replaced_atomically() {
        let rules = ruleset("portal", "10.99.0.0/24", &[]);
        let lines: Vec<&str> = rules.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            &lines[..3],
            &[
                "table ip portal",
                "delete table ip portal",
                "table ip portal {"
            ],
            "create-then-delete keeps the first boot from failing"
        );
    }

    #[test]
    fn an_empty_ruleset_is_still_a_valid_table() {
        let rules = ruleset("portal", "10.99.0.0/24", &[]);
        assert!(rules.contains("chain prerouting"));
        assert!(rules.contains("chain postrouting"));
        assert_eq!(rules.matches("dnat to").count(), 0);
    }

    #[test]
    fn return_traffic_is_masqueraded_into_the_tunnel() {
        let rules = ruleset("portal", "10.99.0.0/24", &[]);
        assert!(rules.contains("ip daddr 10.99.0.0/24 masquerade"));
    }

    #[test]
    fn braces_balance_so_nft_can_parse_it() {
        let rules = ruleset(
            "portal",
            "10.99.0.0/24",
            &[forward(Protocol::Tcp, 25565, 2)],
        );
        assert_eq!(rules.matches('{').count(), rules.matches('}').count());
    }

    #[test]
    fn the_table_name_is_configurable() {
        let rules = ruleset("custom", "10.99.0.0/24", &[]);
        assert!(rules.contains("table ip custom {"));
        assert!(!rules.contains("table ip portal"));
    }
}
