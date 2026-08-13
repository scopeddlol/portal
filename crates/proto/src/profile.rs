//! Game profiles: data, not code.
//!
//! A profile declares the ports a game needs and how they should be published
//! in DNS. Profiles compose — a service can select `minecraft-java` *and*
//! `simple-voice-chat`, and the union of their port templates is what gets
//! allocated on the edge and forwarded down the tunnel. Adding a new game is a
//! YAML file, not a code change.

use crate::model::Protocol;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Profiles that only make sense attached to another one (voice chat,
    /// query/rcon add-ons) are marked so the UI can list them separately.
    #[serde(default)]
    pub addon: bool,
    pub ports: Vec<PortTemplate>,
    /// Free-form operator notes surfaced in the UI when the profile is used.
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortTemplate {
    /// Stable id, unique within the profile. Persisted on the port mapping so
    /// reconciliation can match an existing row to its template.
    pub id: String,
    pub label: String,
    pub protocol: Protocol,
    pub default_port: u16,
    /// Optional ports are off unless the operator enables them.
    #[serde(default)]
    pub optional: bool,
    /// Some clients can't be told about a non-default port (Bedrock's default
    /// UDP 19132, for one), so allocation must not silently move them.
    #[serde(default)]
    pub edge_port_fixed: bool,
    /// When set, the gateway publishes an SRV record so compatible clients can
    /// connect to a bare hostname with no port.
    #[serde(default)]
    pub srv: Option<SrvSpec>,
    /// How to tell the game server about its public address, when the server
    /// needs to advertise one to clients.
    #[serde(default)]
    pub server_config: Option<ServerConfigHint>,
}

/// An SRV record to publish alongside the service's A record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SrvSpec {
    /// Leading-underscore service label, e.g. `_minecraft`.
    pub service: String,
    /// Leading-underscore protocol label, e.g. `_tcp`.
    pub proto: String,
    #[serde(default)]
    pub priority: u16,
    #[serde(default)]
    pub weight: u16,
}

impl SrvSpec {
    /// The record name under a service's FQDN, e.g.
    /// `_minecraft._tcp.mc.example.com`.
    pub fn record_name(&self, service_fqdn: &str) -> String {
        format!("{}.{}.{}", self.service, self.proto, service_fqdn)
    }
}

/// A key the operator must set in a server config file for proxying to work.
///
/// The agent can apply these, but only when the operator opts in per service
/// and confirms the change — silently rewriting someone's server config is a
/// good way to lose their trust and their world settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigHint {
    /// Path relative to the game server directory.
    pub file: String,
    pub key: String,
    /// Supports `{host}` and `{edge_port}` placeholders.
    pub value_template: String,
    #[serde(default)]
    pub explanation: Option<String>,
}

impl ServerConfigHint {
    pub fn render(&self, host: &str, edge_port: u16) -> String {
        self.value_template
            .replace("{host}", host)
            .replace("{edge_port}", &edge_port.to_string())
    }
}

/// All profiles known to the gateway, keyed by id.
#[derive(Debug, Clone, Default)]
pub struct ProfileSet {
    profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("no profile named `{0}`")]
    Unknown(String),
    #[error("profile `{0}` declares duplicate port template id `{1}`")]
    DuplicateTemplate(String, String),
    #[error("profiles `{0}` and `{1}` both define {2} port {3}; they cannot be combined")]
    ConflictingPort(String, String, Protocol, u16),
    #[error("failed to read profile directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
}

/// A port template together with the profile that contributed it.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedTemplate<'a> {
    pub profile: &'a Profile,
    pub template: &'a PortTemplate,
}

impl ProfileSet {
    /// Load every `*.yaml` in a directory. Files are validated on load so a
    /// malformed profile fails at startup rather than when someone tries to
    /// create a service at 2am.
    pub fn load_dir(dir: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let mut profiles = BTreeMap::new();
        for entry in std::fs::read_dir(dir.as_ref())? {
            let path = entry?.path();
            let is_yaml = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "yaml" || e == "yml");
            if !is_yaml {
                continue;
            }
            let raw = std::fs::read_to_string(&path)?;
            let profile: Profile =
                serde_yaml::from_str(&raw).map_err(|source| ProfileError::Parse {
                    path: path.display().to_string(),
                    source,
                })?;
            profile.validate()?;
            profiles.insert(profile.id.clone(), profile);
        }
        Ok(Self { profiles })
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.values()
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Expand a service's profile selection into the port templates it needs.
    ///
    /// Rejects combinations that collide on the local side: two profiles both
    /// wanting UDP 24454 on the same machine cannot both be served, and it is
    /// better to say so at creation time than to produce a half-working
    /// tunnel.
    pub fn resolve(&self, ids: &[String]) -> Result<Vec<ResolvedTemplate<'_>>, ProfileError> {
        let mut resolved: Vec<ResolvedTemplate<'_>> = Vec::new();
        for id in ids {
            let profile = self
                .profiles
                .get(id)
                .ok_or_else(|| ProfileError::Unknown(id.clone()))?;
            for template in &profile.ports {
                if let Some(existing) = resolved.iter().find(|r| {
                    r.template.protocol == template.protocol
                        && r.template.default_port == template.default_port
                }) {
                    return Err(ProfileError::ConflictingPort(
                        existing.profile.id.clone(),
                        profile.id.clone(),
                        template.protocol,
                        template.default_port,
                    ));
                }
                resolved.push(ResolvedTemplate { profile, template });
            }
        }
        Ok(resolved)
    }
}

impl Profile {
    fn validate(&self) -> Result<(), ProfileError> {
        let mut seen = std::collections::HashSet::new();
        for port in &self.ports {
            if !seen.insert(&port.id) {
                return Err(ProfileError::DuplicateTemplate(
                    self.id.clone(),
                    port.id.clone(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, ports: &[(&str, Protocol, u16)]) -> Profile {
        Profile {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            addon: false,
            notes: vec![],
            ports: ports
                .iter()
                .map(|(tid, proto, port)| PortTemplate {
                    id: tid.to_string(),
                    label: tid.to_string(),
                    protocol: *proto,
                    default_port: *port,
                    optional: false,
                    edge_port_fixed: false,
                    srv: None,
                    server_config: None,
                })
                .collect(),
        }
    }

    fn set(profiles: Vec<Profile>) -> ProfileSet {
        ProfileSet {
            profiles: profiles.into_iter().map(|p| (p.id.clone(), p)).collect(),
        }
    }

    #[test]
    fn composes_game_and_addon_ports() {
        let set = set(vec![
            profile("minecraft-java", &[("game", Protocol::Tcp, 25565)]),
            profile("simple-voice-chat", &[("voice", Protocol::Udp, 24454)]),
        ]);
        let resolved = set
            .resolve(&[
                "minecraft-java".to_string(),
                "simple-voice-chat".to_string(),
            ])
            .expect("profiles compose");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[1].template.default_port, 24454);
    }

    #[test]
    fn tcp_and_udp_on_the_same_number_do_not_collide() {
        let set = set(vec![
            profile("game", &[("game", Protocol::Tcp, 25565)]),
            profile("voice", &[("voice", Protocol::Udp, 25565)]),
        ]);
        assert!(set
            .resolve(&["game".to_string(), "voice".to_string()])
            .is_ok());
    }

    #[test]
    fn rejects_same_protocol_port_collisions() {
        let set = set(vec![
            profile("a", &[("game", Protocol::Udp, 24454)]),
            profile("b", &[("voice", Protocol::Udp, 24454)]),
        ]);
        let err = set
            .resolve(&["a".to_string(), "b".to_string()])
            .expect_err("collision must be rejected");
        assert!(matches!(err, ProfileError::ConflictingPort(..)));
    }

    #[test]
    fn unknown_profile_is_named_in_the_error() {
        let set = set(vec![]);
        let err = set.resolve(&["nope".to_string()]).expect_err("unknown");
        assert_eq!(err.to_string(), "no profile named `nope`");
    }

    #[test]
    fn srv_record_name_is_fully_qualified() {
        let srv = SrvSpec {
            service: "_minecraft".into(),
            proto: "_tcp".into(),
            priority: 0,
            weight: 5,
        };
        assert_eq!(
            srv.record_name("mc.example.com"),
            "_minecraft._tcp.mc.example.com"
        );
    }

    #[test]
    fn config_hint_renders_public_address() {
        let hint = ServerConfigHint {
            file: "config/voicechat/voicechat-server.properties".into(),
            key: "voice_host".into(),
            value_template: "{host}:{edge_port}".into(),
            explanation: None,
        };
        assert_eq!(hint.render("mc.example.com", 30001), "mc.example.com:30001");
    }
}
