//! Turning "I run Minecraft with voice chat" into concrete forwards.
//!
//! This is the step between the web UI and everything that touches the
//! machine. It expands the selected profiles into port templates, decides the
//! local and edge port for each, and derives what the operator will see: the
//! address players type, the config keys they still have to set, and the DNS
//! records the reconciler should publish.
//!
//! Planning is a pure function of (profiles, request, current allocations). It
//! writes nothing, so the caller can show a plan before committing to it, and
//! a failure part-way through leaves no half-allocated ports behind.

use crate::alloc::{AllocError, PortAllocator, PortRequest};
use crate::dns::{service_records, DnsRecord, SrvBinding};
use portal_proto::api::{ConfigAction, CreateServiceRequest};
use portal_proto::model::{Endpoint, PortMapping, Protocol, Service};
use portal_proto::profile::{ProfileError, ProfileSet, ResolvedTemplate};
use std::net::Ipv4Addr;
use time::OffsetDateTime;
use uuid::Uuid;

/// How a port template is named outside its own profile: `profile-id/template-id`.
///
/// Template ids are only unique within a profile — several profiles call their
/// main port `game` — so anything crossing the boundary (request overrides,
/// the `template_id` persisted on a mapping) uses the qualified form.
pub fn template_key(profile_id: &str, template_id: &str) -> String {
    format!("{profile_id}/{template_id}")
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Alloc(#[from] AllocError),
    #[error("`{0}` is not a valid subdomain label")]
    InvalidSubdomain(String),
    #[error("no port template `{0}` in the selected profiles")]
    UnknownTemplate(String),
    #[error("`{0}` and `{1}` would both listen on {2} port {3} on the agent")]
    LocalPortConflict(String, String, Protocol, u16),
    #[error("this selection exposes no ports; enable at least one")]
    NoPorts,
}

/// A planned service: everything needed to persist it, program the edge, and
/// render it in the UI.
#[derive(Debug, Clone)]
pub struct ServicePlan {
    pub service: Service,
    pub fqdn: String,
    pub mappings: Vec<PortMapping>,
    /// What players type, aligned index-for-index with `mappings`.
    pub endpoints: Vec<Endpoint>,
    pub config_actions: Vec<ConfigAction>,
    pub dns: Vec<DnsRecord>,
}

/// Gateway-wide settings that every plan is made against.
#[derive(Debug, Clone, Copy)]
pub struct Planner<'a> {
    pub profiles: &'a ProfileSet,
    /// The Cloudflare zone the gateway manages, e.g. `example.com`.
    pub zone: &'a str,
    /// Public address of the VPS, what the A records point at.
    pub edge_ip: Ipv4Addr,
}

impl Planner<'_> {
    pub fn plan(
        &self,
        allocator: &mut PortAllocator,
        req: &CreateServiceRequest,
        now: OffsetDateTime,
    ) -> Result<ServicePlan, PlanError> {
        let subdomain = normalize_subdomain(&req.subdomain)?;
        let resolved = self.profiles.resolve(&req.profiles)?;

        // Reject typos in the request before allocating anything: a key that
        // matches no template means the operator asked for something they did
        // not get, and silently ignoring it produces a service missing a port.
        for key in req
            .enabled_optional_ports
            .iter()
            .chain(req.local_port_overrides.keys())
        {
            if !resolved
                .iter()
                .any(|r| &template_key(&r.profile.id, &r.template.id) == key)
            {
                return Err(PlanError::UnknownTemplate(key.clone()));
            }
        }

        let selected = self.select_ports(&resolved, req);
        if selected.is_empty() {
            return Err(PlanError::NoPorts);
        }
        check_local_conflicts(&selected)?;

        let service = Service {
            id: Uuid::new_v4(),
            agent_id: req.agent_id,
            name: req.name.clone(),
            subdomain,
            profiles: req.profiles.clone(),
            enabled: true,
            created_at: now,
        };
        let fqdn = service.fqdn(self.zone);

        let edge_ports = allocate_all(allocator, &selected)?;

        let mut mappings = Vec::with_capacity(selected.len());
        let mut endpoints = Vec::with_capacity(selected.len());
        let mut config_actions = Vec::new();
        let mut srvs = Vec::new();

        for (port, edge_port) in selected.iter().zip(edge_ports) {
            mappings.push(PortMapping {
                id: Uuid::new_v4(),
                service_id: service.id,
                template_id: port.key.clone(),
                protocol: port.template.protocol,
                local_port: port.local_port,
                edge_port,
            });
            endpoints.push(Endpoint {
                host: fqdn.clone(),
                port: edge_port,
                protocol: port.template.protocol,
                port_implied_by_srv: port.template.srv.is_some(),
            });
            if let Some(hint) = &port.template.server_config {
                config_actions.push(ConfigAction {
                    file: hint.file.clone(),
                    key: hint.key.clone(),
                    value: hint.render(&fqdn, edge_port),
                    explanation: hint.explanation.clone(),
                });
            }
            if let Some(spec) = &port.template.srv {
                srvs.push(SrvBinding { spec, edge_port });
            }
        }

        let dns = service_records(&fqdn, self.edge_ip, srvs);

        Ok(ServicePlan {
            service,
            fqdn,
            mappings,
            endpoints,
            config_actions,
            dns,
        })
    }

    /// Drop optional templates the operator did not enable, and apply local
    /// port overrides for the ones that stay.
    fn select_ports<'t>(
        &self,
        resolved: &[ResolvedTemplate<'t>],
        req: &CreateServiceRequest,
    ) -> Vec<SelectedPort<'t>> {
        let mut selected = Vec::new();
        for r in resolved {
            let key = template_key(&r.profile.id, &r.template.id);
            if r.template.optional && !req.enabled_optional_ports.contains(&key) {
                continue;
            }
            let local_port = req
                .local_port_overrides
                .get(&key)
                .copied()
                .unwrap_or(r.template.default_port);
            selected.push(SelectedPort {
                key,
                template: r.template,
                local_port,
            });
        }
        selected
    }
}

/// A port template that made it into the plan, with its local port resolved.
#[derive(Debug, Clone)]
struct SelectedPort<'t> {
    /// Qualified `profile-id/template-id`, as persisted on the mapping.
    key: String,
    template: &'t portal_proto::profile::PortTemplate,
    local_port: u16,
}

/// Two listeners on the same protocol and port on the agent's machine cannot
/// both work. Profiles are checked for this at their defaults, but local port
/// overrides can create a collision the profile set never saw.
fn check_local_conflicts(selected: &[SelectedPort<'_>]) -> Result<(), PlanError> {
    for (i, port) in selected.iter().enumerate() {
        if let Some(other) = selected[..i].iter().find(|p| {
            p.template.protocol == port.template.protocol && p.local_port == port.local_port
        }) {
            return Err(PlanError::LocalPortConflict(
                other.key.clone(),
                port.key.clone(),
                port.template.protocol,
                port.local_port,
            ));
        }
    }
    Ok(())
}

/// Allocate an edge port for every selected template, all or nothing.
///
/// Fixed ports go first: a Bedrock service must have UDP 19132 or fail, so it
/// would be perverse for a flexible template in the same request to take that
/// number as its preferred port and force the failure.
fn allocate_all(
    allocator: &mut PortAllocator,
    selected: &[SelectedPort<'_>],
) -> Result<Vec<u16>, PlanError> {
    let mut ports: Vec<Option<u16>> = vec![None; selected.len()];
    let mut result = Ok(());

    for fixed_pass in [true, false] {
        for (i, port) in selected.iter().enumerate() {
            if port.template.edge_port_fixed != fixed_pass {
                continue;
            }
            // The template's own default is the natural public port even when
            // the server listens somewhere else locally.
            let req = PortRequest {
                protocol: port.template.protocol,
                preferred: port.template.default_port,
                fixed: fixed_pass,
            };
            match allocator.allocate(req) {
                Ok(edge) => ports[i] = Some(edge),
                Err(e) => {
                    result = Err(PlanError::Alloc(e));
                    break;
                }
            }
        }
        if result.is_err() {
            break;
        }
    }

    if let Err(e) = result {
        // Planning is meant to be free of side effects; hand back anything
        // this attempt took so the allocator can be reused as-is.
        for (i, port) in selected.iter().enumerate() {
            if let Some(edge) = ports[i] {
                allocator.release(port.template.protocol, edge);
            }
        }
        return Err(e);
    }

    Ok(ports
        .into_iter()
        .map(|p| p.expect("all passes ran"))
        .collect())
}

/// DNS labels are case-insensitive and letter/digit/hyphen only; `@` means the
/// zone apex. Normalising rather than rejecting on case keeps `MC` from being
/// an error when the operator meant `mc`.
fn normalize_subdomain(raw: &str) -> Result<String, PlanError> {
    let label = raw.trim().to_ascii_lowercase();
    if label == "@" {
        return Ok(label);
    }
    let valid = !label.is_empty()
        && label.len() <= 63
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-');
    if valid {
        Ok(label)
    } else {
        Err(PlanError::InvalidSubdomain(raw.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::EdgePortRange;
    use std::collections::BTreeMap;

    const IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 10);

    fn profiles() -> ProfileSet {
        ProfileSet::load_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../profiles"))
            .expect("the shipped profiles must load")
    }

    fn request(subdomain: &str, ids: &[&str]) -> CreateServiceRequest {
        CreateServiceRequest {
            agent_id: Uuid::new_v4(),
            name: "test".into(),
            subdomain: subdomain.into(),
            profiles: ids.iter().map(|s| s.to_string()).collect(),
            local_port_overrides: BTreeMap::new(),
            enabled_optional_ports: Vec::new(),
        }
    }

    fn planner(profiles: &ProfileSet) -> Planner<'_> {
        Planner {
            profiles,
            zone: "example.com",
            edge_ip: IP,
        }
    }

    fn allocator() -> PortAllocator {
        PortAllocator::new(EdgePortRange::DEFAULT)
    }

    fn plan(
        profiles: &ProfileSet,
        allocator: &mut PortAllocator,
        req: &CreateServiceRequest,
    ) -> Result<ServicePlan, PlanError> {
        planner(profiles).plan(allocator, req, OffsetDateTime::UNIX_EPOCH)
    }

    #[test]
    fn minecraft_with_voice_chat_gets_both_ports_on_one_subdomain() {
        let profiles = profiles();
        let plan = plan(
            &profiles,
            &mut allocator(),
            &request("mc", &["minecraft-java", "simple-voice-chat"]),
        )
        .expect("the headline combination must plan");

        assert_eq!(plan.fqdn, "mc.example.com");
        assert_eq!(plan.mappings.len(), 2, "game and voice, no optional ports");
        assert_eq!(plan.mappings[0].template_id, "minecraft-java/game");
        assert_eq!(plan.mappings[0].edge_port, 25565);
        assert_eq!(plan.mappings[1].template_id, "simple-voice-chat/voice");
        assert_eq!(plan.mappings[1].edge_port, 24454);
        assert!(plan.endpoints.iter().all(|e| e.host == "mc.example.com"));
    }

    #[test]
    fn java_endpoint_hides_the_port_because_srv_carries_it() {
        let profiles = profiles();
        let plan = plan(
            &profiles,
            &mut allocator(),
            &request("mc", &["minecraft-java"]),
        )
        .unwrap();
        assert_eq!(plan.endpoints[0].to_string(), "mc.example.com");
        assert!(plan
            .dns
            .iter()
            .any(|r| r.name() == "_minecraft._tcp.mc.example.com"));
    }

    #[test]
    fn a_second_java_server_still_reads_as_a_bare_hostname() {
        let profiles = profiles();
        let mut alloc = allocator();
        plan(&profiles, &mut alloc, &request("mc", &["minecraft-java"])).unwrap();
        let second = plan(&profiles, &mut alloc, &request("smp", &["minecraft-java"])).unwrap();

        assert_eq!(second.mappings[0].edge_port, 30000, "25565 was taken");
        assert_eq!(
            second.mappings[0].local_port, 25565,
            "the game still binds 25565 at home"
        );
        assert_eq!(
            second.endpoints[0].to_string(),
            "smp.example.com",
            "SRV is what makes the moved port invisible to Java clients"
        );
    }

    #[test]
    fn voice_config_action_carries_the_public_address() {
        let profiles = profiles();
        let mut alloc = allocator();
        alloc.reserve(Protocol::Udp, 24454); // another service already has it
        let plan = plan(
            &profiles,
            &mut alloc,
            &request("mc", &["minecraft-java", "simple-voice-chat"]),
        )
        .unwrap();

        let action = plan
            .config_actions
            .iter()
            .find(|a| a.key == "voice_host")
            .expect("voice chat must tell the operator what to set");
        assert_eq!(action.value, "mc.example.com:30000");
    }

    #[test]
    fn bedrock_refuses_to_move_off_its_fixed_port() {
        let profiles = profiles();
        let mut alloc = allocator();
        plan(
            &profiles,
            &mut alloc,
            &request("be", &["minecraft-bedrock"]),
        )
        .unwrap();
        let err = plan(
            &profiles,
            &mut alloc,
            &request("be2", &["minecraft-bedrock"]),
        )
        .expect_err("a second Bedrock service cannot have 19132");
        assert!(matches!(
            err,
            PlanError::Alloc(AllocError::FixedPortTaken { port: 19132, .. })
        ));
    }

    #[test]
    fn a_failed_plan_releases_every_port_it_took() {
        let profiles = profiles();
        let mut alloc = allocator();
        // Bedrock's optional IPv6 listener is flexible and allocates fine; the
        // fixed game port is the one that fails.
        alloc.reserve(Protocol::Udp, 19132);
        let mut req = request("be", &["minecraft-bedrock"]);
        req.enabled_optional_ports = vec!["minecraft-bedrock/game-ipv6".into()];

        assert!(plan(&profiles, &mut alloc, &req).is_err());
        assert!(
            !alloc.is_taken(Protocol::Udp, 19133),
            "the optional port must not stay allocated to a service that was never created"
        );
    }

    #[test]
    fn optional_ports_stay_off_until_asked_for() {
        let profiles = profiles();
        let mut req = request("mc", &["minecraft-java"]);
        let without = plan(&profiles, &mut allocator(), &req).unwrap();
        assert_eq!(without.mappings.len(), 1);

        req.enabled_optional_ports = vec!["minecraft-java/rcon".into()];
        let with = plan(&profiles, &mut allocator(), &req).unwrap();
        assert_eq!(with.mappings.len(), 2);
        assert!(with
            .mappings
            .iter()
            .any(|m| m.template_id == "minecraft-java/rcon"));
    }

    #[test]
    fn local_port_override_moves_only_the_local_side() {
        let profiles = profiles();
        let mut req = request("mc", &["minecraft-java"]);
        req.local_port_overrides
            .insert("minecraft-java/game".into(), 25570);
        let plan = plan(&profiles, &mut allocator(), &req).unwrap();

        assert_eq!(plan.mappings[0].local_port, 25570);
        assert_eq!(
            plan.mappings[0].edge_port, 25565,
            "the public port should stay the one players expect"
        );
    }

    #[test]
    fn overrides_that_collide_locally_are_rejected() {
        let profiles = profiles();
        let mut req = request("mc", &["minecraft-java", "simple-voice-chat"]);
        // Point voice at the Java query port and enable that port too, so both
        // want UDP 25565 on the same machine.
        req.enabled_optional_ports = vec!["minecraft-java/query".into()];
        req.local_port_overrides
            .insert("simple-voice-chat/voice".into(), 25565);

        let err = plan(&profiles, &mut allocator(), &req).expect_err("collision must surface");
        assert!(matches!(
            err,
            PlanError::LocalPortConflict(_, _, Protocol::Udp, 25565)
        ));
    }

    #[test]
    fn a_mistyped_template_key_is_an_error_not_a_silent_no_op() {
        let profiles = profiles();
        let mut req = request("mc", &["minecraft-java"]);
        req.enabled_optional_ports = vec!["minecraft-java/rcon-typo".into()];
        assert!(matches!(
            plan(&profiles, &mut allocator(), &req),
            Err(PlanError::UnknownTemplate(_))
        ));
    }

    #[test]
    fn subdomains_are_normalized_and_validated() {
        let profiles = profiles();
        let plan = plan(
            &profiles,
            &mut allocator(),
            &request("  MC  ", &["minecraft-java"]),
        )
        .unwrap();
        assert_eq!(plan.fqdn, "mc.example.com");

        for bad in ["-mc", "mc-", "mc.smp", "mc_smp", ""] {
            assert!(
                matches!(
                    super::normalize_subdomain(bad),
                    Err(PlanError::InvalidSubdomain(_))
                ),
                "`{bad}` should be rejected"
            );
        }
    }

    #[test]
    fn apex_service_uses_the_zone_itself() {
        let profiles = profiles();
        let plan = plan(
            &profiles,
            &mut allocator(),
            &request("@", &["minecraft-java"]),
        )
        .unwrap();
        assert_eq!(plan.fqdn, "example.com");
    }

    #[test]
    fn dns_for_a_service_is_one_a_record_plus_its_srvs() {
        let profiles = profiles();
        let plan = plan(
            &profiles,
            &mut allocator(),
            &request("mc", &["minecraft-java", "simple-voice-chat"]),
        )
        .unwrap();
        assert_eq!(plan.dns.len(), 2, "voice is advertised via config, not DNS");
        assert!(plan.dns.iter().all(|r| !r.proxied()));
    }
}
