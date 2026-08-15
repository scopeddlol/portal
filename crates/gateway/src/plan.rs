//! Turning a service and its mappings into what the world sees.
//!
//! There is no game-specific knowledge here any more. A service is a
//! subdomain; a mapping is "public port -> address and port behind a node".
//! Everything else is derived: the address a player types, and the DNS records
//! that make it resolve.

use crate::alloc::{AllocError, PortAllocator, PortRequest};
use crate::dns::{service_records, DnsRecord, SrvBinding};
use portal_proto::api::{AddPortRequest, PortView};
use portal_proto::model::{Endpoint, PortMapping, Service, SrvSpec};
use std::net::Ipv4Addr;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Alloc(#[from] AllocError),
    #[error("`{0}` is not a valid subdomain label")]
    InvalidSubdomain(String),
    #[error("`{0}` is not a valid address for the server behind the node")]
    InvalidLocalHost(String),
    #[error("port numbers must be between 1 and 65535")]
    InvalidPort,
}

/// DNS labels are case-insensitive and letter/digit/hyphen only; `@` means the
/// zone apex. Normalising rather than rejecting on case keeps `MC` from being
/// an error when the operator meant `mc`.
pub fn normalize_subdomain(raw: &str) -> Result<String, PlanError> {
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

/// Accept a hostname or IP for the server behind the node.
///
/// Deliberately permissive about *which* address: the agent may be able to
/// reach things this process cannot, which is the entire point of it being
/// over there. What it rejects is input that could not be an address at all.
pub fn normalize_local_host(raw: &str) -> Result<String, PlanError> {
    let host = raw.trim().to_ascii_lowercase();
    let plausible = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if plausible {
        Ok(host)
    } else {
        Err(PlanError::InvalidLocalHost(raw.to_string()))
    }
}

/// Decide the public port for a new mapping and reserve it.
///
/// An explicit choice is honoured or refused; left blank, the local port is
/// tried first so the common single-server case looks like an ordinary
/// forwarded port, and the configured range is the fallback.
pub fn allocate_edge_port(
    allocator: &mut PortAllocator,
    req: &AddPortRequest,
) -> Result<u16, PlanError> {
    if req.local_port == 0 {
        return Err(PlanError::InvalidPort);
    }
    let port = match req.edge_port {
        Some(0) | None => {
            allocator.allocate(PortRequest::flexible(req.protocol, req.local_port))?
        }
        Some(explicit) => allocator.allocate(PortRequest::fixed(req.protocol, explicit))?,
    };
    Ok(port)
}

/// What a stored service looks like to the UI and to the DNS reconciler.
#[derive(Debug, Clone)]
pub struct ServiceDescription {
    pub fqdn: String,
    pub ports: Vec<PortView>,
    pub dns: Vec<DnsRecord>,
}

/// Derive a service's public face from what is actually stored.
///
/// The database is the source of truth, so what the UI shows and what the
/// reconciler publishes cannot drift from what is really forwarded.
pub fn describe_service(
    service: &Service,
    mappings: &[PortMapping],
    zone: &str,
    edge_ip: Ipv4Addr,
) -> ServiceDescription {
    let fqdn = service.fqdn(zone);
    let mut ports = Vec::with_capacity(mappings.len());
    let mut srvs = Vec::new();

    for mapping in mappings {
        let endpoint = Endpoint {
            host: fqdn.clone(),
            port: mapping.edge_port,
            protocol: mapping.protocol,
            port_implied_by_srv: mapping.srv.is_some(),
        };
        ports.push(PortView {
            connect: endpoint.to_string(),
            endpoint,
            mapping: mapping.clone(),
        });
        if let Some(spec) = &mapping.srv {
            srvs.push(SrvBinding {
                spec: spec.clone(),
                edge_port: mapping.edge_port,
            });
        }
    }

    ServiceDescription {
        fqdn: fqdn.clone(),
        dns: service_records(&fqdn, edge_ip, srvs),
        ports,
    }
}

/// The SRV spec a request asks for, if any.
pub fn srv_for(req: &AddPortRequest) -> Option<SrvSpec> {
    req.minecraft_srv.then(SrvSpec::minecraft_java)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::EdgePortRange;
    use portal_proto::model::Protocol;
    use time::OffsetDateTime;
    use uuid::Uuid;

    const IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 10);

    fn service(subdomain: &str) -> Service {
        Service {
            id: Uuid::new_v4(),
            node_id: Uuid::new_v4(),
            name: subdomain.into(),
            subdomain: subdomain.into(),
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn mapping(service: &Service, port: u16, edge: u16, srv: bool) -> PortMapping {
        PortMapping {
            id: Uuid::new_v4(),
            service_id: service.id,
            protocol: Protocol::Tcp,
            local_host: "192.168.1.50".into(),
            local_port: port,
            edge_port: edge,
            srv: srv.then(SrvSpec::minecraft_java),
        }
    }

    fn request(port: u16, edge: Option<u16>) -> AddPortRequest {
        AddPortRequest {
            protocol: Protocol::Tcp,
            local_host: "192.168.1.50".into(),
            local_port: port,
            edge_port: edge,
            minecraft_srv: false,
        }
    }

    #[test]
    fn subdomains_are_normalized_and_validated() {
        assert_eq!(normalize_subdomain("  MC  ").unwrap(), "mc");
        assert_eq!(normalize_subdomain("@").unwrap(), "@");
        for bad in ["-mc", "mc-", "mc.smp", "mc_smp", ""] {
            assert!(
                normalize_subdomain(bad).is_err(),
                "`{bad}` should be rejected"
            );
        }
    }

    #[test]
    fn lan_addresses_are_accepted_as_the_target() {
        assert_eq!(
            normalize_local_host("192.168.1.50").unwrap(),
            "192.168.1.50"
        );
        assert_eq!(normalize_local_host(" NAS.local ").unwrap(), "nas.local");
        for bad in ["", "192.168.1.50:25565", "has space", "http://x"] {
            assert!(
                normalize_local_host(bad).is_err(),
                "`{bad}` should be rejected"
            );
        }
    }

    #[test]
    fn a_blank_public_port_prefers_the_local_one() {
        let mut alloc = PortAllocator::new(EdgePortRange::DEFAULT);
        assert_eq!(
            allocate_edge_port(&mut alloc, &request(25565, None)).unwrap(),
            25565
        );
    }

    #[test]
    fn the_second_server_on_the_same_port_falls_back_to_the_range() {
        let mut alloc = PortAllocator::new(EdgePortRange::DEFAULT);
        allocate_edge_port(&mut alloc, &request(25565, None)).unwrap();
        let second = allocate_edge_port(&mut alloc, &request(25565, None)).unwrap();
        assert_eq!(second, 30000);
    }

    #[test]
    fn an_explicit_public_port_is_refused_rather_than_moved() {
        let mut alloc = PortAllocator::new(EdgePortRange::DEFAULT);
        allocate_edge_port(&mut alloc, &request(25565, Some(25565))).unwrap();
        let err = allocate_edge_port(&mut alloc, &request(25566, Some(25565)))
            .expect_err("asking for a taken port must fail, not silently move");
        assert!(matches!(
            err,
            PlanError::Alloc(AllocError::FixedPortTaken { .. })
        ));
    }

    #[test]
    fn ten_servers_on_one_node_each_get_their_own_public_port() {
        let mut alloc = PortAllocator::new(EdgePortRange::DEFAULT);
        let ports: Vec<u16> = (0..10)
            .map(|_| allocate_edge_port(&mut alloc, &request(25565, None)).unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = ports.iter().collect();
        assert_eq!(unique.len(), 10, "every server needs its own public port");
        assert_eq!(ports[0], 25565, "the first one still looks normal");
    }

    #[test]
    fn srv_hides_the_port_so_ten_servers_are_all_bare_names() {
        let svc = service("mc");
        let described = describe_service(
            &svc,
            &[mapping(&svc, 25565, 30007, true)],
            "example.com",
            IP,
        );
        assert_eq!(described.ports[0].connect, "mc.example.com");
        assert!(described
            .dns
            .iter()
            .any(|r| r.name() == "_minecraft._tcp.mc.example.com"));
    }

    #[test]
    fn without_srv_the_player_gets_told_the_port() {
        let svc = service("mc");
        let described = describe_service(
            &svc,
            &[mapping(&svc, 25565, 30007, false)],
            "example.com",
            IP,
        );
        assert_eq!(described.ports[0].connect, "mc.example.com:30007");
        assert_eq!(described.dns.len(), 1, "just the A record");
    }

    #[test]
    fn a_service_with_no_ports_still_resolves() {
        let svc = service("mc");
        let described = describe_service(&svc, &[], "example.com", IP);
        assert_eq!(described.dns.len(), 1);
        assert!(described.ports.is_empty());
    }
}
