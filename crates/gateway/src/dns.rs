//! Desired DNS state, and the diff that gets Cloudflare there.
//!
//! Records here are always **DNS-only** (grey cloud). Cloudflare's proxy
//! carries HTTP/HTTPS on a fixed set of ports and will not carry TCP 25565 or
//! UDP 24454, so an orange-clouded record does not hide the origin here — it
//! breaks the game. The VPS's public IP is what players resolve to, and that
//! is the whole point: it is a host you are willing to publish, and the home
//! IP behind the tunnel never appears in the zone.
//!
//! Nothing in this module makes an HTTP request. It computes what the zone
//! should look like and what has to change; the Cloudflare client applies it.

use portal_proto::profile::SrvSpec;
use std::net::Ipv4Addr;

/// Short by DNS standards, on purpose: rebuilding a VPS changes the edge IP,
/// and players should not be stuck on a dead address for an hour.
pub const DEFAULT_TTL: u32 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsRecord {
    A {
        name: String,
        address: Ipv4Addr,
        ttl: u32,
    },
    Srv {
        /// Fully qualified, e.g. `_minecraft._tcp.mc.example.com`.
        name: String,
        /// Hostname carrying the A record, i.e. the service's own FQDN.
        target: String,
        port: u16,
        priority: u16,
        weight: u16,
        ttl: u32,
    },
}

impl DnsRecord {
    pub fn name(&self) -> &str {
        match self {
            DnsRecord::A { name, .. } | DnsRecord::Srv { name, .. } => name,
        }
    }

    /// Record type as Cloudflare's API spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            DnsRecord::A { .. } => "A",
            DnsRecord::Srv { .. } => "SRV",
        }
    }

    /// Always false. See the module docs: proxying game ports is not something
    /// Cloudflare's CDN does, so this is a property of the design rather than
    /// a setting anyone should be able to flip.
    pub const fn proxied(&self) -> bool {
        false
    }

    /// Records are identified by type and name when diffing; content
    /// differences become updates rather than a delete/create pair, which
    /// avoids a window where the name does not resolve at all.
    fn identity(&self) -> (&'static str, &str) {
        (self.kind(), self.name())
    }
}

/// An SRV record to publish for one allocated port.
#[derive(Debug, Clone, Copy)]
pub struct SrvBinding<'a> {
    pub spec: &'a SrvSpec,
    pub edge_port: u16,
}

/// Everything that should exist in the zone for one service.
pub fn service_records<'a>(
    fqdn: &str,
    edge_ip: Ipv4Addr,
    srvs: impl IntoIterator<Item = SrvBinding<'a>>,
) -> Vec<DnsRecord> {
    let mut records = vec![DnsRecord::A {
        name: fqdn.to_string(),
        address: edge_ip,
        ttl: DEFAULT_TTL,
    }];
    for binding in srvs {
        records.push(DnsRecord::Srv {
            name: binding.spec.record_name(fqdn),
            target: fqdn.to_string(),
            port: binding.edge_port,
            priority: binding.spec.priority,
            weight: binding.spec.weight,
            ttl: DEFAULT_TTL,
        });
    }
    records
}

/// A record already present in the zone, with the id needed to change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingRecord {
    pub id: String,
    pub record: DnsRecord,
}

/// The API calls that would bring the zone in line with the desired state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnsPlan {
    pub create: Vec<DnsRecord>,
    /// `(record id, new content)`.
    pub update: Vec<(String, DnsRecord)>,
    /// Record ids to remove.
    pub delete: Vec<String>,
}

impl DnsPlan {
    pub fn is_empty(&self) -> bool {
        self.create.is_empty() && self.update.is_empty() && self.delete.is_empty()
    }
}

/// True when `record_name` is a name this service owns: its own FQDN, or
/// something beneath it like `_minecraft._tcp.mc.example.com`.
///
/// Reconciliation deletes records it does not recognise, so this is the fence
/// that keeps it from touching the rest of somebody's zone — the MX records
/// for their mail, the apex A record for their website.
pub fn owned_by_service(record_name: &str, service_fqdn: &str) -> bool {
    record_name == service_fqdn
        || record_name
            .strip_suffix(service_fqdn)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Diff the desired records for one service against the zone as it stands.
///
/// `zone_records` may be the whole zone; anything outside the service's own
/// names is ignored rather than deleted.
pub fn reconcile_service(
    service_fqdn: &str,
    desired: &[DnsRecord],
    zone_records: &[ExistingRecord],
) -> DnsPlan {
    let mine: Vec<&ExistingRecord> = zone_records
        .iter()
        .filter(|e| owned_by_service(e.record.name(), service_fqdn))
        .collect();

    let mut plan = DnsPlan::default();
    let mut matched: Vec<bool> = vec![false; mine.len()];

    for want in desired {
        // First not-yet-matched record with the same type and name.
        let hit = mine
            .iter()
            .enumerate()
            .find(|(i, e)| !matched[*i] && e.record.identity() == want.identity());
        match hit {
            Some((i, existing)) => {
                matched[i] = true;
                if &existing.record != want {
                    plan.update.push((existing.id.clone(), want.clone()));
                }
            }
            None => plan.create.push(want.clone()),
        }
    }

    // Whatever is left under this service's names is stale: a port that moved,
    // a profile that was removed, or a duplicate of a record we just matched.
    for (i, existing) in mine.iter().enumerate() {
        if !matched[i] {
            plan.delete.push(existing.id.clone());
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 10);

    fn srv_spec() -> SrvSpec {
        SrvSpec {
            service: "_minecraft".into(),
            proto: "_tcp".into(),
            priority: 0,
            weight: 5,
        }
    }

    fn existing(id: &str, record: DnsRecord) -> ExistingRecord {
        ExistingRecord {
            id: id.to_string(),
            record,
        }
    }

    #[test]
    fn a_record_is_never_proxied() {
        let records = service_records("mc.example.com", IP, []);
        assert_eq!(records.len(), 1);
        assert!(
            !records[0].proxied(),
            "orange cloud cannot carry a game port"
        );
    }

    #[test]
    fn srv_points_at_the_hostname_and_the_allocated_port() {
        let spec = srv_spec();
        let records = service_records(
            "mc.example.com",
            IP,
            [SrvBinding {
                spec: &spec,
                edge_port: 30001,
            }],
        );
        let srv = &records[1];
        assert_eq!(srv.name(), "_minecraft._tcp.mc.example.com");
        assert!(matches!(
            srv,
            DnsRecord::Srv { target, port: 30001, .. } if target == "mc.example.com"
        ));
    }

    #[test]
    fn an_in_sync_zone_produces_no_calls() {
        let desired = service_records("mc.example.com", IP, []);
        let zone = vec![existing("rec1", desired[0].clone())];
        assert!(reconcile_service("mc.example.com", &desired, &zone).is_empty());
    }

    #[test]
    fn missing_records_are_created() {
        let spec = srv_spec();
        let desired = service_records(
            "mc.example.com",
            IP,
            [SrvBinding {
                spec: &spec,
                edge_port: 25565,
            }],
        );
        let plan = reconcile_service("mc.example.com", &desired, &[]);
        assert_eq!(plan.create.len(), 2);
        assert!(plan.update.is_empty() && plan.delete.is_empty());
    }

    #[test]
    fn a_changed_edge_ip_is_an_update_not_a_delete_and_create() {
        let desired = service_records("mc.example.com", IP, []);
        let zone = vec![existing(
            "rec1",
            DnsRecord::A {
                name: "mc.example.com".into(),
                address: Ipv4Addr::new(198, 51, 100, 7),
                ttl: DEFAULT_TTL,
            },
        )];
        let plan = reconcile_service("mc.example.com", &desired, &zone);
        assert_eq!(plan.update.len(), 1);
        assert_eq!(plan.update[0].0, "rec1");
        assert!(plan.delete.is_empty(), "the name must never stop resolving");
    }

    #[test]
    fn stale_records_under_the_service_are_deleted() {
        let desired = service_records("mc.example.com", IP, []);
        let zone = vec![
            existing("rec1", desired[0].clone()),
            existing(
                "rec2",
                DnsRecord::Srv {
                    name: "_minecraft._tcp.mc.example.com".into(),
                    target: "mc.example.com".into(),
                    port: 30001,
                    priority: 0,
                    weight: 5,
                    ttl: DEFAULT_TTL,
                },
            ),
        ];
        let plan = reconcile_service("mc.example.com", &desired, &zone);
        assert_eq!(plan.delete, vec!["rec2".to_string()]);
    }

    #[test]
    fn records_elsewhere_in_the_zone_are_left_alone() {
        let desired = service_records("mc.example.com", IP, []);
        let zone = vec![
            existing("rec1", desired[0].clone()),
            existing(
                "apex",
                DnsRecord::A {
                    name: "example.com".into(),
                    address: Ipv4Addr::new(198, 51, 100, 1),
                    ttl: 300,
                },
            ),
            existing(
                "other-service",
                DnsRecord::A {
                    name: "notmc.example.com".into(),
                    address: IP,
                    ttl: DEFAULT_TTL,
                },
            ),
        ];
        let plan = reconcile_service("mc.example.com", &desired, &zone);
        assert!(
            plan.is_empty(),
            "reconciling one service must not touch the rest of the zone: {plan:?}"
        );
    }

    #[test]
    fn ownership_check_requires_a_label_boundary() {
        assert!(owned_by_service("mc.example.com", "mc.example.com"));
        assert!(owned_by_service(
            "_minecraft._tcp.mc.example.com",
            "mc.example.com"
        ));
        assert!(!owned_by_service("notmc.example.com", "mc.example.com"));
        assert!(!owned_by_service("example.com", "mc.example.com"));
    }

    #[test]
    fn duplicate_records_collapse_to_one() {
        let desired = service_records("mc.example.com", IP, []);
        let zone = vec![
            existing("rec1", desired[0].clone()),
            existing("dupe", desired[0].clone()),
        ];
        let plan = reconcile_service("mc.example.com", &desired, &zone);
        assert_eq!(plan.delete, vec!["dupe".to_string()]);
        assert!(plan.create.is_empty() && plan.update.is_empty());
    }
}
