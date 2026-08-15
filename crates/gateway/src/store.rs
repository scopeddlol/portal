//! Persistent state, in SQLite.
//!
//! Values are stored in forms a human can read with the `sqlite3` CLI — UUIDs
//! and timestamps as text. When someone's tunnel is down at midnight, being
//! able to read the database without this binary is worth more than a few
//! bytes per row.
//!
//! `port_mappings` carries `UNIQUE(protocol, edge_port)`. The allocator
//! already prevents collisions, but the allocator is rebuilt from this table
//! at startup; the constraint is what makes a bug there fail loudly instead of
//! quietly pointing two services at one port.

use crate::net::Ipv4Net;
use crate::token;
use portal_proto::api::Forward;
use portal_proto::model::{Node, PortMapping, Protocol, Service, SrvSpec};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Mutex;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0} is already taken")]
    Conflict(String),
    #[error("not found")]
    NotFound,
    #[error("the tunnel subnet has no free addresses left")]
    SubnetExhausted,
    #[error("stored value is corrupt: {0}")]
    Corrupt(String),
    #[error(
        "this database predates the node/service/port model and cannot be upgraded in place; \
         delete it (or the portal-data volume) and start again"
    )]
    IncompatibleSchema,
}

type Result<T> = std::result::Result<T, StoreError>;

/// Handle to the gateway's database.
///
/// SQLite calls are synchronous and held under a mutex. The workload is a
/// household's worth of game servers and a web UI that one person clicks, so
/// the simplicity is worth more than the concurrency; if that ever stops being
/// true, this is the seam to put a pool behind.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL keeps a reader (the web UI) from blocking a writer (an agent
        // registering). Harmless on the in-memory database used by tests.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.check_compatible()?;
        self.conn().execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS nodes (
                id             TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                key_hash       TEXT NOT NULL UNIQUE,
                public_key     TEXT,
                tunnel_ip      TEXT NOT NULL UNIQUE,
                last_handshake TEXT,
                created_at     TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS services (
                id         TEXT PRIMARY KEY,
                node_id    TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                name       TEXT NOT NULL,
                subdomain  TEXT NOT NULL UNIQUE,
                enabled    INTEGER NOT NULL DEFAULT 1,
                dns_synced INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS port_mappings (
                id          TEXT PRIMARY KEY,
                service_id  TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                protocol    TEXT NOT NULL,
                local_host  TEXT NOT NULL,
                local_port  INTEGER NOT NULL,
                edge_port   INTEGER NOT NULL,
                srv_service TEXT,
                srv_proto   TEXT,
                UNIQUE (protocol, edge_port)
            );

            CREATE INDEX IF NOT EXISTS idx_mappings_service ON port_mappings(service_id);
            CREATE INDEX IF NOT EXISTS idx_services_node ON services(node_id);
            "#,
        )?;
        Ok(())
    }

    /// Refuse a database written by an older, incompatible schema.
    ///
    /// `CREATE TABLE IF NOT EXISTS` leaves an existing table alone, so the
    /// migration below would run happily against the old shape and then fail
    /// deep inside an index with "no such column: node_id" — which tells an
    /// operator nothing about what to do. Detect it up front and say so.
    fn check_compatible(&self) -> Result<()> {
        let conn = self.conn();
        let services_exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'services'",
            [],
            |row| row.get::<_, i64>(0),
        )? > 0;
        if !services_exists {
            return Ok(());
        }
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('services')")?;
        let has_node_id = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "node_id");
        if has_node_id {
            Ok(())
        } else {
            Err(StoreError::IncompatibleSchema)
        }
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- nodes ----------------------------------------------------------

    /// Create a node and mint the key its agent will authenticate with.
    ///
    /// The tunnel address is assigned now and never changes, so the forwards
    /// pointing at it survive the agent restarting, upgrading, or being moved
    /// to another machine.
    pub fn create_node(
        &self,
        name: &str,
        subnet: Ipv4Net,
        gateway_ip: Ipv4Addr,
        now: OffsetDateTime,
    ) -> Result<(Node, String)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        let mut taken = vec![gateway_ip];
        {
            let mut stmt = tx.prepare("SELECT tunnel_ip FROM nodes")?;
            for ip in stmt.query_map([], |row| row.get::<_, String>(0))? {
                taken.push(
                    ip?.parse()
                        .map_err(|_| StoreError::Corrupt("tunnel_ip".into()))?,
                );
            }
        }
        let tunnel_ip = subnet
            .next_free(&taken)
            .map_err(|_| StoreError::SubnetExhausted)?;

        let node = Node {
            id: Uuid::new_v4(),
            name: name.to_string(),
            public_key: None,
            tunnel_ip,
            last_handshake: None,
            created_at: now,
        };
        let key = token::generate();

        tx.execute(
            "INSERT INTO nodes (id, name, key_hash, tunnel_ip, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                node.id.to_string(),
                node.name,
                token::hash(&key),
                node.tunnel_ip.to_string(),
                fmt_time(now)?
            ],
        )?;
        tx.commit()?;
        Ok((node, key))
    }

    pub fn list_nodes(&self) -> Result<Vec<Node>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, public_key, tunnel_ip, last_handshake, created_at
             FROM nodes ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_node)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    pub fn get_node(&self, id: Uuid) -> Result<Node> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, name, public_key, tunnel_ip, last_handshake, created_at
             FROM nodes WHERE id = ?1",
            params![id.to_string()],
            row_to_node,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?
    }

    /// Resolve a node from the key its agent presents.
    pub fn authenticate_node(&self, key: &str) -> Result<Node> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, name, public_key, tunnel_ip, last_handshake, created_at
             FROM nodes WHERE key_hash = ?1",
            params![token::hash(key)],
            row_to_node,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?
    }

    /// Record the tunnel identity an agent just generated.
    ///
    /// Agents are stateless and make a fresh keypair every start, so this
    /// overwrites whatever was there. The node — and its address — persist.
    pub fn set_node_public_key(&self, id: Uuid, public_key: &str) -> Result<()> {
        let n = self.conn().execute(
            "UPDATE nodes SET public_key = ?1 WHERE id = ?2",
            params![public_key, id.to_string()],
        )?;
        (n > 0).then_some(()).ok_or(StoreError::NotFound)
    }

    pub fn record_handshake(&self, id: Uuid, at: OffsetDateTime) -> Result<()> {
        self.conn().execute(
            "UPDATE nodes SET last_handshake = ?1 WHERE id = ?2",
            params![fmt_time(at)?, id.to_string()],
        )?;
        Ok(())
    }

    pub fn delete_node(&self, id: Uuid) -> Result<()> {
        let n = self
            .conn()
            .execute("DELETE FROM nodes WHERE id = ?1", params![id.to_string()])?;
        (n > 0).then_some(()).ok_or(StoreError::NotFound)
    }

    // ---- services -------------------------------------------------------

    pub fn insert_service(&self, service: &Service) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO services (id, node_id, name, subdomain, enabled, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    service.id.to_string(),
                    service.node_id.to_string(),
                    service.name,
                    service.subdomain,
                    service.enabled as i64,
                    fmt_time(service.created_at)?
                ],
            )
            .map_err(map_conflict("that subdomain"))?;
        Ok(())
    }

    pub fn list_services(&self) -> Result<Vec<Service>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, node_id, name, subdomain, enabled, created_at
             FROM services ORDER BY subdomain",
        )?;
        let rows = stmt.query_map([], row_to_service)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    pub fn get_service(&self, id: Uuid) -> Result<Service> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, node_id, name, subdomain, enabled, created_at
             FROM services WHERE id = ?1",
            params![id.to_string()],
            row_to_service,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?
    }

    pub fn set_service_enabled(&self, id: Uuid, enabled: bool) -> Result<()> {
        let n = self.conn().execute(
            "UPDATE services SET enabled = ?1 WHERE id = ?2",
            params![enabled as i64, id.to_string()],
        )?;
        (n > 0).then_some(()).ok_or(StoreError::NotFound)
    }

    pub fn set_dns_synced(&self, id: Uuid, synced: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE services SET dns_synced = ?1 WHERE id = ?2",
            params![synced as i64, id.to_string()],
        )?;
        Ok(())
    }

    pub fn is_dns_synced(&self, id: Uuid) -> Result<bool> {
        let conn = self.conn();
        let synced: i64 = conn
            .query_row(
                "SELECT dns_synced FROM services WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        Ok(synced != 0)
    }

    pub fn delete_service(&self, id: Uuid) -> Result<()> {
        let n = self.conn().execute(
            "DELETE FROM services WHERE id = ?1",
            params![id.to_string()],
        )?;
        (n > 0).then_some(()).ok_or(StoreError::NotFound)
    }

    // ---- port mappings --------------------------------------------------

    pub fn add_port(&self, mapping: &PortMapping) -> Result<()> {
        let (srv_service, srv_proto) = match &mapping.srv {
            Some(spec) => (Some(spec.service.clone()), Some(spec.proto.clone())),
            None => (None, None),
        };
        self.conn()
            .execute(
                "INSERT INTO port_mappings
                     (id, service_id, protocol, local_host, local_port, edge_port,
                      srv_service, srv_proto)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    mapping.id.to_string(),
                    mapping.service_id.to_string(),
                    mapping.protocol.as_str(),
                    mapping.local_host,
                    mapping.local_port,
                    mapping.edge_port,
                    srv_service,
                    srv_proto
                ],
            )
            .map_err(map_conflict("that public port"))?;
        Ok(())
    }

    pub fn delete_port(&self, id: Uuid) -> Result<Uuid> {
        let conn = self.conn();
        let service_id: String = conn
            .query_row(
                "SELECT service_id FROM port_mappings WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        conn.execute(
            "DELETE FROM port_mappings WHERE id = ?1",
            params![id.to_string()],
        )?;
        service_id
            .parse()
            .map_err(|_| StoreError::Corrupt("service id".into()))
    }

    pub fn ports_for_service(&self, service_id: Uuid) -> Result<Vec<PortMapping>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, service_id, protocol, local_host, local_port, edge_port,
                    srv_service, srv_proto
             FROM port_mappings WHERE service_id = ?1 ORDER BY protocol, edge_port",
        )?;
        let rows = stmt.query_map(params![service_id.to_string()], row_to_mapping)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    /// Every `(protocol, edge_port)` in use, for rebuilding the allocator at
    /// startup. Disabled services keep their ports: turning a server off for
    /// the winter should not mean losing its address.
    pub fn taken_edge_ports(&self) -> Result<Vec<(Protocol, u16)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT protocol, edge_port FROM port_mappings")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u16>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(proto, port)| Ok((parse_protocol(&proto)?, port)))
            .collect()
    }

    /// All enabled forwards for one node, which is exactly its assignment.
    pub fn forwards_for_node(&self, node_id: Uuid) -> Result<Vec<Forward>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT m.protocol, m.edge_port, m.local_host, m.local_port
             FROM port_mappings m
             JOIN services s ON s.id = m.service_id
             WHERE s.node_id = ?1 AND s.enabled = 1
             ORDER BY m.protocol, m.edge_port",
        )?;
        let rows = stmt.query_map(params![node_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u16>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u16>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(proto, edge_port, local_host, local_port)| {
                Ok(Forward {
                    protocol: parse_protocol(&proto)?,
                    // The agent listens on the same port inside the tunnel that
                    // players hit outside it, so DNAT is a pure address change
                    // and a packet capture reads the same on both sides.
                    tunnel_port: edge_port,
                    local_host,
                    local_port,
                })
            })
            .collect()
    }

    /// Everything nftables needs: enabled mappings joined to the tunnel
    /// address they belong to.
    pub fn active_forwards(&self) -> Result<Vec<ActiveForward>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n.tunnel_ip, m.protocol, m.edge_port, s.id
             FROM port_mappings m
             JOIN services s ON s.id = m.service_id
             JOIN nodes n    ON n.id = s.node_id
             WHERE s.enabled = 1
             ORDER BY m.protocol, m.edge_port",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u16>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(ip, proto, edge_port, service_id)| {
                Ok(ActiveForward {
                    tunnel_ip: ip
                        .parse()
                        .map_err(|_| StoreError::Corrupt("tunnel_ip".into()))?,
                    protocol: parse_protocol(&proto)?,
                    edge_port,
                    service_id: service_id
                        .parse()
                        .map_err(|_| StoreError::Corrupt("service id".into()))?,
                })
            })
            .collect()
    }
}

/// One live forward, flattened across service and node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveForward {
    pub tunnel_ip: Ipv4Addr,
    pub protocol: Protocol,
    pub edge_port: u16,
    pub service_id: Uuid,
}

fn map_conflict(what: &'static str) -> impl Fn(rusqlite::Error) -> StoreError {
    move |e| match e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::Conflict(what.to_string())
        }
        other => StoreError::Sqlite(other),
    }
}

fn fmt_time(t: OffsetDateTime) -> Result<String> {
    t.format(&Rfc3339)
        .map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn parse_time(raw: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn parse_protocol(raw: &str) -> Result<Protocol> {
    raw.parse()
        .map_err(|_| StoreError::Corrupt(format!("protocol `{raw}`")))
}

fn row_to_node(row: &Row<'_>) -> rusqlite::Result<Result<Node>> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let public_key: Option<String> = row.get(2)?;
    let tunnel_ip: String = row.get(3)?;
    let last_handshake: Option<String> = row.get(4)?;
    let created_at: String = row.get(5)?;
    Ok((|| {
        Ok(Node {
            id: id
                .parse()
                .map_err(|_| StoreError::Corrupt("node id".into()))?,
            name,
            public_key,
            tunnel_ip: tunnel_ip
                .parse()
                .map_err(|_| StoreError::Corrupt("tunnel_ip".into()))?,
            last_handshake: last_handshake.as_deref().map(parse_time).transpose()?,
            created_at: parse_time(&created_at)?,
        })
    })())
}

fn row_to_service(row: &Row<'_>) -> rusqlite::Result<Result<Service>> {
    let id: String = row.get(0)?;
    let node_id: String = row.get(1)?;
    let name: String = row.get(2)?;
    let subdomain: String = row.get(3)?;
    let enabled: i64 = row.get(4)?;
    let created_at: String = row.get(5)?;
    Ok((|| {
        Ok(Service {
            id: id
                .parse()
                .map_err(|_| StoreError::Corrupt("service id".into()))?,
            node_id: node_id
                .parse()
                .map_err(|_| StoreError::Corrupt("node id".into()))?,
            name,
            subdomain,
            enabled: enabled != 0,
            created_at: parse_time(&created_at)?,
        })
    })())
}

fn row_to_mapping(row: &Row<'_>) -> rusqlite::Result<Result<PortMapping>> {
    let id: String = row.get(0)?;
    let service_id: String = row.get(1)?;
    let protocol: String = row.get(2)?;
    let local_host: String = row.get(3)?;
    let local_port: u16 = row.get(4)?;
    let edge_port: u16 = row.get(5)?;
    let srv_service: Option<String> = row.get(6)?;
    let srv_proto: Option<String> = row.get(7)?;
    Ok((|| {
        Ok(PortMapping {
            id: id
                .parse()
                .map_err(|_| StoreError::Corrupt("mapping id".into()))?,
            service_id: service_id
                .parse()
                .map_err(|_| StoreError::Corrupt("service id".into()))?,
            protocol: parse_protocol(&protocol)?,
            local_host,
            local_port,
            edge_port,
            srv: match (srv_service, srv_proto) {
                (Some(service), Some(proto)) => Some(SrvSpec {
                    service,
                    proto,
                    priority: 0,
                    weight: 5,
                }),
                _ => None,
            },
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn subnet() -> Ipv4Net {
        "10.99.0.0/24".parse().unwrap()
    }

    fn gateway_ip() -> Ipv4Addr {
        Ipv4Addr::new(10, 99, 0, 1)
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn node(store: &Store, name: &str) -> (Node, String) {
        store
            .create_node(name, subnet(), gateway_ip(), now())
            .unwrap()
    }

    fn service(store: &Store, node: &Node, subdomain: &str) -> Service {
        let service = Service {
            id: Uuid::new_v4(),
            node_id: node.id,
            name: subdomain.to_string(),
            subdomain: subdomain.to_string(),
            enabled: true,
            created_at: now(),
        };
        store.insert_service(&service).unwrap();
        service
    }

    fn port(service: &Service, host: &str, local: u16, edge: u16) -> PortMapping {
        PortMapping {
            id: Uuid::new_v4(),
            service_id: service.id,
            protocol: Protocol::Tcp,
            local_host: host.to_string(),
            local_port: local,
            edge_port: edge,
            srv: None,
        }
    }

    #[test]
    fn nodes_get_a_stable_address_and_a_key() {
        let store = store();
        let (first, key_a) = node(&store, "one");
        let (second, key_b) = node(&store, "two");

        assert_eq!(first.tunnel_ip, Ipv4Addr::new(10, 99, 0, 2));
        assert_eq!(second.tunnel_ip, Ipv4Addr::new(10, 99, 0, 3));
        assert_ne!(key_a, key_b);
        assert_eq!(store.authenticate_node(&key_a).unwrap().id, first.id);
    }

    #[test]
    fn node_keys_are_not_stored_in_the_clear() {
        let store = store();
        let (_, key) = node(&store, "box");
        let stored: String = store
            .conn()
            .query_row("SELECT key_hash FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, key);
        assert!(matches!(
            store.authenticate_node("wrong"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn a_restarting_agent_keeps_its_address_but_changes_its_key() {
        let store = store();
        let (n, _) = node(&store, "box");
        store.set_node_public_key(n.id, "FIRST-BOOT-KEY").unwrap();
        store.set_node_public_key(n.id, "SECOND-BOOT-KEY").unwrap();

        let reloaded = store.get_node(n.id).unwrap();
        assert_eq!(reloaded.public_key.as_deref(), Some("SECOND-BOOT-KEY"));
        assert_eq!(
            reloaded.tunnel_ip, n.tunnel_ip,
            "the address must survive, or every forward pointing at it breaks"
        );
    }

    #[test]
    fn one_node_can_front_many_machines_on_its_lan() {
        let store = store();
        let (n, _) = node(&store, "box");

        for i in 0..10u16 {
            let svc = service(&store, &n, &format!("mc{i}"));
            store
                .add_port(&port(
                    &svc,
                    &format!("192.168.1.{}", 50 + i),
                    25565,
                    30000 + i,
                ))
                .unwrap();
        }

        let forwards = store.forwards_for_node(n.id).unwrap();
        assert_eq!(forwards.len(), 10);
        assert_eq!(forwards[0].local_host, "192.168.1.50");
        assert_eq!(forwards[9].local_host, "192.168.1.59");
        assert!(
            forwards.iter().all(|f| f.local_port == 25565),
            "ten servers can all use the standard port on their own machines"
        );
    }

    #[test]
    fn a_service_can_hold_several_ports() {
        let store = store();
        let (n, _) = node(&store, "box");
        let svc = service(&store, &n, "mc");

        store
            .add_port(&port(&svc, "192.168.1.50", 25565, 25565))
            .unwrap();
        let mut voice = port(&svc, "192.168.1.50", 24454, 24454);
        voice.protocol = Protocol::Udp;
        store.add_port(&voice).unwrap();

        assert_eq!(store.ports_for_service(svc.id).unwrap().len(), 2);
    }

    #[test]
    fn two_mappings_cannot_share_a_public_port() {
        let store = store();
        let (n, _) = node(&store, "box");
        let a = service(&store, &n, "mc");
        let b = service(&store, &n, "smp");
        store
            .add_port(&port(&a, "192.168.1.50", 25565, 25565))
            .unwrap();
        assert!(matches!(
            store.add_port(&port(&b, "192.168.1.51", 25565, 25565)),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn duplicate_subdomains_are_refused() {
        let store = store();
        let (n, _) = node(&store, "box");
        service(&store, &n, "mc");
        let clash = Service {
            id: Uuid::new_v4(),
            node_id: n.id,
            name: "again".into(),
            subdomain: "mc".into(),
            enabled: true,
            created_at: now(),
        };
        assert!(matches!(
            store.insert_service(&clash),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn deleting_a_node_takes_its_services_and_ports_with_it() {
        let store = store();
        let (n, _) = node(&store, "box");
        let svc = service(&store, &n, "mc");
        store
            .add_port(&port(&svc, "192.168.1.50", 25565, 25565))
            .unwrap();

        store.delete_node(n.id).unwrap();
        assert!(store.list_services().unwrap().is_empty());
        assert!(
            store.taken_edge_ports().unwrap().is_empty(),
            "ports must return to the pool"
        );
    }

    #[test]
    fn a_disabled_service_keeps_its_ports_but_stops_being_forwarded() {
        let store = store();
        let (n, _) = node(&store, "box");
        let svc = service(&store, &n, "mc");
        store
            .add_port(&port(&svc, "192.168.1.50", 25565, 25565))
            .unwrap();
        store.set_service_enabled(svc.id, false).unwrap();

        assert!(store.forwards_for_node(n.id).unwrap().is_empty());
        assert!(store
            .taken_edge_ports()
            .unwrap()
            .contains(&(Protocol::Tcp, 25565)));
    }

    #[test]
    fn deleting_a_port_frees_it_and_names_its_service() {
        let store = store();
        let (n, _) = node(&store, "box");
        let svc = service(&store, &n, "mc");
        let mapping = port(&svc, "192.168.1.50", 25565, 25565);
        store.add_port(&mapping).unwrap();

        assert_eq!(store.delete_port(mapping.id).unwrap(), svc.id);
        assert!(store.taken_edge_ports().unwrap().is_empty());
    }

    #[test]
    fn srv_settings_survive_a_round_trip() {
        let store = store();
        let (n, _) = node(&store, "box");
        let svc = service(&store, &n, "mc");
        let mut mapping = port(&svc, "192.168.1.50", 25565, 30001);
        mapping.srv = Some(SrvSpec::minecraft_java());
        store.add_port(&mapping).unwrap();

        let loaded = &store.ports_for_service(svc.id).unwrap()[0];
        assert_eq!(loaded.srv.as_ref().unwrap().service, "_minecraft");
    }

    #[test]
    fn active_forwards_carry_the_tunnel_address_for_dnat() {
        let store = store();
        let (n, _) = node(&store, "box");
        let svc = service(&store, &n, "mc");
        store
            .add_port(&port(&svc, "192.168.1.50", 25565, 25565))
            .unwrap();

        let active = store.active_forwards().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].tunnel_ip, n.tunnel_ip);
    }

    #[test]
    fn handshakes_drive_the_online_flag() {
        let store = store();
        let (n, _) = node(&store, "box");
        assert!(!store.get_node(n.id).unwrap().is_online(now()));

        store.record_handshake(n.id, now()).unwrap();
        assert!(store.get_node(n.id).unwrap().is_online(now()));
        assert!(!store
            .get_node(n.id)
            .unwrap()
            .is_online(now() + time::Duration::hours(1)));
    }

    #[test]
    fn a_database_from_the_old_model_is_refused_with_an_explanation() {
        let dir = std::env::temp_dir().join(format!("portal-old-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("portal.db");
        {
            // The shape before nodes existed: services keyed by agent_id.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE services (id TEXT PRIMARY KEY, agent_id TEXT, subdomain TEXT);",
            )
            .unwrap();
        }

        let err = match Store::open(&path) {
            Err(e) => e,
            Ok(_) => panic!("an unupgradable database must not be opened"),
        };
        assert!(matches!(err, StoreError::IncompatibleSchema));
        assert!(
            err.to_string().contains("delete it"),
            "the message has to say what to do: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_survives_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("portal-test-{}", Uuid::new_v4()));
        let path = dir.join("portal.db");
        {
            let store = Store::open(&path).unwrap();
            node(&store, "box");
        }
        assert_eq!(Store::open(&path).unwrap().list_nodes().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
