//! Persistent state, in SQLite.
//!
//! Two decisions worth knowing about:
//!
//! Values are stored in forms a human can read with the `sqlite3` CLI — UUIDs
//! and timestamps as text, profile lists as JSON. When someone's tunnel is
//! down at midnight, being able to read the database without this binary is
//! worth more than a few bytes per row.
//!
//! `port_mappings` carries `UNIQUE(protocol, edge_port)`. The allocator
//! already prevents collisions, but the allocator is rebuilt from this table
//! at startup; the constraint is what makes a bug there fail loudly instead of
//! quietly pointing two services at one port.

use crate::net::Ipv4Net;
use crate::token;
use portal_proto::api::Forward;
use portal_proto::model::{Agent, PortMapping, Protocol, Service};
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
    #[error("`{0}` is already taken")]
    Conflict(String),
    #[error("not found")]
    NotFound,
    #[error("enrollment token is not valid, or has already been used")]
    BadEnrollmentToken,
    #[error("the tunnel subnet has no free addresses left")]
    SubnetExhausted,
    #[error("stored value is corrupt: {0}")]
    Corrupt(String),
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
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL keeps a reader (the web UI polling) from blocking a writer (an
        // agent enrolling). Harmless on the in-memory database used by tests.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS agents (
                id             TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                public_key     TEXT NOT NULL UNIQUE,
                tunnel_ip      TEXT NOT NULL UNIQUE,
                agent_key_hash TEXT NOT NULL,
                last_handshake TEXT,
                created_at     TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS enrollment_tokens (
                token_hash TEXT PRIMARY KEY,
                label      TEXT NOT NULL,
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                used_at    TEXT,
                used_by    TEXT
            );

            CREATE TABLE IF NOT EXISTS services (
                id         TEXT PRIMARY KEY,
                agent_id   TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
                name       TEXT NOT NULL,
                subdomain  TEXT NOT NULL UNIQUE,
                profiles   TEXT NOT NULL,
                enabled    INTEGER NOT NULL DEFAULT 1,
                dns_synced INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS port_mappings (
                id          TEXT PRIMARY KEY,
                service_id  TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                template_id TEXT NOT NULL,
                protocol    TEXT NOT NULL,
                local_port  INTEGER NOT NULL,
                edge_port   INTEGER NOT NULL,
                UNIQUE (protocol, edge_port)
            );

            CREATE INDEX IF NOT EXISTS idx_mappings_service ON port_mappings(service_id);
            CREATE INDEX IF NOT EXISTS idx_services_agent ON services(agent_id);
            "#,
        )?;
        Ok(())
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- agents ---------------------------------------------------------

    /// Mint an enrollment token. It is single-use and short-lived: it is going
    /// to be pasted into a chat window or typed off a screen, and it buys a
    /// permanent place in the tunnel.
    pub fn create_enrollment_token(
        &self,
        label: &str,
        now: OffsetDateTime,
        valid_for: time::Duration,
    ) -> Result<String> {
        let plaintext = token::generate();
        self.conn().execute(
            "INSERT INTO enrollment_tokens (token_hash, label, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                token::hash(&plaintext),
                label,
                fmt_time(now)?,
                fmt_time(now + valid_for)?
            ],
        )?;
        Ok(plaintext)
    }

    /// Exchange an enrollment token for a place in the tunnel subnet.
    ///
    /// Token check, address allocation and agent creation happen in one
    /// transaction, so two agents racing on the same token cannot both win and
    /// cannot both get the same address.
    pub fn enroll_agent(
        &self,
        enrollment_token: &str,
        name: &str,
        public_key: &str,
        subnet: Ipv4Net,
        gateway_ip: Ipv4Addr,
        now: OffsetDateTime,
    ) -> Result<(Agent, String)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let hash = token::hash(enrollment_token);

        let expires: Option<String> = tx
            .query_row(
                "SELECT expires_at FROM enrollment_tokens
                 WHERE token_hash = ?1 AND used_at IS NULL",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        let expires = expires.ok_or(StoreError::BadEnrollmentToken)?;
        if parse_time(&expires)? < now {
            return Err(StoreError::BadEnrollmentToken);
        }

        let mut taken = vec![gateway_ip];
        let mut stmt = tx.prepare("SELECT tunnel_ip FROM agents")?;
        for ip in stmt.query_map([], |row| row.get::<_, String>(0))? {
            taken.push(
                ip?.parse()
                    .map_err(|_| StoreError::Corrupt("tunnel_ip".into()))?,
            );
        }
        drop(stmt);
        let tunnel_ip = subnet
            .next_free(&taken)
            .map_err(|_| StoreError::SubnetExhausted)?;

        let agent = Agent {
            id: Uuid::new_v4(),
            name: name.to_string(),
            public_key: public_key.to_string(),
            tunnel_ip,
            last_handshake: None,
            created_at: now,
        };
        let agent_key = token::generate();

        tx.execute(
            "INSERT INTO agents (id, name, public_key, tunnel_ip, agent_key_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                agent.id.to_string(),
                agent.name,
                agent.public_key,
                agent.tunnel_ip.to_string(),
                token::hash(&agent_key),
                fmt_time(now)?
            ],
        )
        .map_err(map_conflict("an agent with that public key"))?;
        tx.execute(
            "UPDATE enrollment_tokens SET used_at = ?1, used_by = ?2 WHERE token_hash = ?3",
            params![fmt_time(now)?, agent.id.to_string(), hash],
        )?;
        tx.commit()?;
        Ok((agent, agent_key))
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, public_key, tunnel_ip, last_handshake, created_at
             FROM agents ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_agent)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Result<Vec<_>>>()
    }

    pub fn get_agent(&self, id: Uuid) -> Result<Agent> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, name, public_key, tunnel_ip, last_handshake, created_at
             FROM agents WHERE id = ?1",
            params![id.to_string()],
            row_to_agent,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?
    }

    /// Resolve an agent from the key it presents. Returns `NotFound` rather
    /// than saying which half was wrong.
    pub fn authenticate_agent(&self, agent_key: &str) -> Result<Agent> {
        let hash = token::hash(agent_key);
        let conn = self.conn();
        conn.query_row(
            "SELECT id, name, public_key, tunnel_ip, last_handshake, created_at
             FROM agents WHERE agent_key_hash = ?1",
            params![hash],
            row_to_agent,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?
    }

    pub fn record_handshake(&self, id: Uuid, at: OffsetDateTime) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET last_handshake = ?1 WHERE id = ?2",
            params![fmt_time(at)?, id.to_string()],
        )?;
        Ok(())
    }

    pub fn delete_agent(&self, id: Uuid) -> Result<()> {
        let n = self
            .conn()
            .execute("DELETE FROM agents WHERE id = ?1", params![id.to_string()])?;
        (n > 0).then_some(()).ok_or(StoreError::NotFound)
    }

    // ---- services -------------------------------------------------------

    /// Persist a planned service and its mappings together.
    ///
    /// The transaction is the point: a service with half its ports written is
    /// worse than no service at all, because the missing half looks like a
    /// game bug rather than a gateway one.
    pub fn insert_service(&self, service: &Service, mappings: &[PortMapping]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO services (id, agent_id, name, subdomain, profiles, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                service.id.to_string(),
                service.agent_id.to_string(),
                service.name,
                service.subdomain,
                serde_json::to_string(&service.profiles)
                    .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                service.enabled as i64,
                fmt_time(service.created_at)?
            ],
        )
        .map_err(map_conflict("that subdomain"))?;

        for m in mappings {
            tx.execute(
                "INSERT INTO port_mappings
                     (id, service_id, template_id, protocol, local_port, edge_port)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    m.id.to_string(),
                    m.service_id.to_string(),
                    m.template_id,
                    m.protocol.as_str(),
                    m.local_port,
                    m.edge_port
                ],
            )
            .map_err(map_conflict("that edge port"))?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_services(&self) -> Result<Vec<Service>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, name, subdomain, profiles, enabled, created_at
             FROM services ORDER BY subdomain",
        )?;
        let rows = stmt.query_map([], row_to_service)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Result<Vec<_>>>()
    }

    pub fn get_service(&self, id: Uuid) -> Result<Service> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, agent_id, name, subdomain, profiles, enabled, created_at
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

    pub fn mappings_for_service(&self, service_id: Uuid) -> Result<Vec<PortMapping>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, service_id, template_id, protocol, local_port, edge_port
             FROM port_mappings WHERE service_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![service_id.to_string()], row_to_mapping)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Result<Vec<_>>>()
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

    /// All enabled forwards for one agent, which is exactly its assignment.
    pub fn forwards_for_agent(&self, agent_id: Uuid) -> Result<Vec<Forward>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT m.protocol, m.edge_port, m.local_port
             FROM port_mappings m
             JOIN services s ON s.id = m.service_id
             WHERE s.agent_id = ?1 AND s.enabled = 1
             ORDER BY m.protocol, m.edge_port",
        )?;
        let rows = stmt.query_map(params![agent_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u16>(1)?,
                row.get::<_, u16>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(proto, edge_port, local_port)| {
                Ok(Forward {
                    protocol: parse_protocol(&proto)?,
                    // The agent listens on the same port inside the tunnel that
                    // players hit outside it, so DNAT is a pure address change
                    // and a packet capture reads the same on both sides.
                    tunnel_port: edge_port,
                    local_host: "127.0.0.1".to_string(),
                    local_port,
                })
            })
            .collect()
    }

    /// Everything nftables and the agent list need, in one pass: enabled
    /// mappings joined to the tunnel address they belong to.
    pub fn active_forwards(&self) -> Result<Vec<ActiveForward>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT a.tunnel_ip, m.protocol, m.edge_port, m.local_port, s.id
             FROM port_mappings m
             JOIN services s ON s.id = m.service_id
             JOIN agents a   ON a.id = s.agent_id
             WHERE s.enabled = 1
             ORDER BY m.protocol, m.edge_port",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u16>(2)?,
                row.get::<_, u16>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(ip, proto, edge_port, local_port, service_id)| {
                Ok(ActiveForward {
                    tunnel_ip: ip
                        .parse()
                        .map_err(|_| StoreError::Corrupt("tunnel_ip".into()))?,
                    protocol: parse_protocol(&proto)?,
                    edge_port,
                    local_port,
                    service_id: service_id
                        .parse()
                        .map_err(|_| StoreError::Corrupt("service id".into()))?,
                })
            })
            .collect()
    }
}

/// One live forward, flattened across service and agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveForward {
    pub tunnel_ip: Ipv4Addr,
    pub protocol: Protocol,
    pub edge_port: u16,
    pub local_port: u16,
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
    match raw {
        "tcp" => Ok(Protocol::Tcp),
        "udp" => Ok(Protocol::Udp),
        other => Err(StoreError::Corrupt(format!("protocol `{other}`"))),
    }
}

fn row_to_agent(row: &Row<'_>) -> rusqlite::Result<Result<Agent>> {
    let id: String = row.get(0)?;
    let tunnel_ip: String = row.get(3)?;
    let last_handshake: Option<String> = row.get(4)?;
    let created_at: String = row.get(5)?;
    Ok((|| {
        Ok(Agent {
            id: id
                .parse()
                .map_err(|_| StoreError::Corrupt("agent id".into()))?,
            name: row.get::<_, String>(1).unwrap_or_default(),
            public_key: row.get::<_, String>(2).unwrap_or_default(),
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
    let agent_id: String = row.get(1)?;
    let name: String = row.get(2)?;
    let subdomain: String = row.get(3)?;
    let profiles: String = row.get(4)?;
    let enabled: i64 = row.get(5)?;
    let created_at: String = row.get(6)?;
    Ok((|| {
        Ok(Service {
            id: id
                .parse()
                .map_err(|_| StoreError::Corrupt("service id".into()))?,
            agent_id: agent_id
                .parse()
                .map_err(|_| StoreError::Corrupt("agent id".into()))?,
            name,
            subdomain,
            profiles: serde_json::from_str(&profiles)
                .map_err(|e| StoreError::Corrupt(e.to_string()))?,
            enabled: enabled != 0,
            created_at: parse_time(&created_at)?,
        })
    })())
}

fn row_to_mapping(row: &Row<'_>) -> rusqlite::Result<Result<PortMapping>> {
    let id: String = row.get(0)?;
    let service_id: String = row.get(1)?;
    let template_id: String = row.get(2)?;
    let protocol: String = row.get(3)?;
    let local_port: u16 = row.get(4)?;
    let edge_port: u16 = row.get(5)?;
    Ok((|| {
        Ok(PortMapping {
            id: id
                .parse()
                .map_err(|_| StoreError::Corrupt("mapping id".into()))?,
            service_id: service_id
                .parse()
                .map_err(|_| StoreError::Corrupt("service id".into()))?,
            template_id,
            protocol: parse_protocol(&protocol)?,
            local_port,
            edge_port,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: time::Duration = time::Duration::hours(24);

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

    fn enroll(store: &Store, name: &str, key: &str) -> (Agent, String) {
        let token = store.create_enrollment_token(name, now(), DAY).unwrap();
        store
            .enroll_agent(&token, name, key, subnet(), gateway_ip(), now())
            .unwrap()
    }

    fn service_for(agent: &Agent, subdomain: &str) -> Service {
        Service {
            id: Uuid::new_v4(),
            agent_id: agent.id,
            name: subdomain.to_string(),
            subdomain: subdomain.to_string(),
            profiles: vec!["minecraft-java".into()],
            enabled: true,
            created_at: now(),
        }
    }

    fn mapping(service: &Service, protocol: Protocol, edge: u16, local: u16) -> PortMapping {
        PortMapping {
            id: Uuid::new_v4(),
            service_id: service.id,
            template_id: "minecraft-java/game".into(),
            protocol,
            local_port: local,
            edge_port: edge,
        }
    }

    #[test]
    fn enrollment_assigns_the_first_free_tunnel_address() {
        let store = store();
        let (first, _) = enroll(&store, "one", "key-one");
        let (second, _) = enroll(&store, "two", "key-two");
        assert_eq!(first.tunnel_ip, Ipv4Addr::new(10, 99, 0, 2));
        assert_eq!(
            second.tunnel_ip,
            Ipv4Addr::new(10, 99, 0, 3),
            "the gateway holds .1"
        );
    }

    #[test]
    fn an_enrollment_token_works_exactly_once() {
        let store = store();
        let token = store.create_enrollment_token("box", now(), DAY).unwrap();
        store
            .enroll_agent(&token, "box", "key-a", subnet(), gateway_ip(), now())
            .expect("first use");
        let err = store
            .enroll_agent(&token, "box2", "key-b", subnet(), gateway_ip(), now())
            .expect_err("second use must fail");
        assert!(matches!(err, StoreError::BadEnrollmentToken));
    }

    #[test]
    fn an_expired_token_is_refused() {
        let store = store();
        let token = store
            .create_enrollment_token("box", now(), time::Duration::hours(1))
            .unwrap();
        let later = now() + time::Duration::hours(2);
        assert!(matches!(
            store.enroll_agent(&token, "box", "k", subnet(), gateway_ip(), later),
            Err(StoreError::BadEnrollmentToken)
        ));
    }

    #[test]
    fn agent_keys_authenticate_and_are_not_stored_in_the_clear() {
        let store = store();
        let (agent, agent_key) = enroll(&store, "box", "key");
        assert_eq!(store.authenticate_agent(&agent_key).unwrap().id, agent.id);
        assert!(matches!(
            store.authenticate_agent("not-the-key"),
            Err(StoreError::NotFound)
        ));

        let stored: String = store
            .conn()
            .query_row("SELECT agent_key_hash FROM agents", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, agent_key, "the database must not hold the key");
    }

    #[test]
    fn a_service_and_its_ports_are_written_together() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        let service = service_for(&agent, "mc");
        let mappings = vec![
            mapping(&service, Protocol::Tcp, 25565, 25565),
            mapping(&service, Protocol::Udp, 24454, 24454),
        ];
        store.insert_service(&service, &mappings).unwrap();

        assert_eq!(store.mappings_for_service(service.id).unwrap().len(), 2);
        assert_eq!(store.list_services().unwrap().len(), 1);
    }

    #[test]
    fn a_half_written_service_leaves_nothing_behind() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        let first = service_for(&agent, "mc");
        store
            .insert_service(&first, &[mapping(&first, Protocol::Tcp, 25565, 25565)])
            .unwrap();

        // Second service claims a free port and then a taken one.
        let second = service_for(&agent, "smp");
        let err = store
            .insert_service(
                &second,
                &[
                    mapping(&second, Protocol::Tcp, 30000, 25565),
                    mapping(&second, Protocol::Tcp, 25565, 25565),
                ],
            )
            .expect_err("the duplicate edge port must be refused");
        assert!(matches!(err, StoreError::Conflict(_)));

        assert_eq!(store.list_services().unwrap().len(), 1);
        assert!(
            !store
                .taken_edge_ports()
                .unwrap()
                .contains(&(Protocol::Tcp, 30000)),
            "the rolled-back service must not leave a port allocated"
        );
    }

    #[test]
    fn duplicate_subdomains_are_refused() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        let first = service_for(&agent, "mc");
        store.insert_service(&first, &[]).unwrap();
        let clash = service_for(&agent, "mc");
        assert!(matches!(
            store.insert_service(&clash, &[]),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn deleting_an_agent_takes_its_services_and_ports_with_it() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        let service = service_for(&agent, "mc");
        store
            .insert_service(&service, &[mapping(&service, Protocol::Tcp, 25565, 25565)])
            .unwrap();

        store.delete_agent(agent.id).unwrap();
        assert!(store.list_services().unwrap().is_empty());
        assert!(
            store.taken_edge_ports().unwrap().is_empty(),
            "ports must return to the pool"
        );
    }

    #[test]
    fn a_disabled_service_keeps_its_ports_but_stops_being_forwarded() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        let service = service_for(&agent, "mc");
        store
            .insert_service(&service, &[mapping(&service, Protocol::Tcp, 25565, 25565)])
            .unwrap();
        store.set_service_enabled(service.id, false).unwrap();

        assert!(store.forwards_for_agent(agent.id).unwrap().is_empty());
        assert!(
            store
                .taken_edge_ports()
                .unwrap()
                .contains(&(Protocol::Tcp, 25565)),
            "turning a server off for the winter should not lose its port"
        );
    }

    #[test]
    fn an_assignment_maps_the_edge_port_to_the_local_one() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        let service = service_for(&agent, "smp");
        store
            .insert_service(&service, &[mapping(&service, Protocol::Tcp, 30000, 25565)])
            .unwrap();

        let forwards = store.forwards_for_agent(agent.id).unwrap();
        assert_eq!(forwards.len(), 1);
        assert_eq!(forwards[0].tunnel_port, 30000);
        assert_eq!(forwards[0].local_port, 25565);
        assert_eq!(forwards[0].local_host, "127.0.0.1");
    }

    #[test]
    fn active_forwards_carry_the_tunnel_address_for_dnat() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        let service = service_for(&agent, "mc");
        store
            .insert_service(&service, &[mapping(&service, Protocol::Udp, 24454, 24454)])
            .unwrap();

        let active = store.active_forwards().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].tunnel_ip, agent.tunnel_ip);
        assert_eq!(active[0].protocol, Protocol::Udp);
    }

    #[test]
    fn handshakes_drive_the_online_flag() {
        let store = store();
        let (agent, _) = enroll(&store, "box", "key");
        assert!(!store.get_agent(agent.id).unwrap().is_online(now()));

        store.record_handshake(agent.id, now()).unwrap();
        assert!(store.get_agent(agent.id).unwrap().is_online(now()));

        let much_later = now() + time::Duration::hours(1);
        assert!(!store.get_agent(agent.id).unwrap().is_online(much_later));
    }

    #[test]
    fn state_survives_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("portal-test-{}", Uuid::new_v4()));
        let path = dir.join("portal.db");
        {
            let store = Store::open(&path).unwrap();
            enroll(&store, "box", "key");
        }
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.list_agents().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
