//! What the agent remembers between runs.
//!
//! The state file holds the agent's WireGuard private key and the API key it
//! was issued at enrollment, so it is written owner-only and nothing ever logs
//! its contents. Losing it means re-enrolling, which is why it is a plain JSON
//! file in a predictable place rather than something clever.

use portal_proto::api::TunnelConfig;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    /// Base URL of the gateway, e.g. `https://portal.example.com`.
    pub gateway_url: String,
    pub agent_id: Uuid,
    /// Bearer credential for the gateway API.
    pub agent_key: String,
    /// The agent's WireGuard private key. Generated locally at enrollment and
    /// never sent anywhere — the gateway only ever sees the public half.
    pub private_key: String,
    pub tunnel: TunnelConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("no agent state at `{0}`; run `portal-agent enroll` first")]
    NotEnrolled(String),
    #[error("failed to read `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("state file `{path}` is not valid: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

impl AgentState {
    pub fn load(path: &Path) -> Result<Self, StateError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StateError::NotEnrolled(path.display().to_string()))
            }
            Err(source) => {
                return Err(StateError::Io {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        serde_json::from_str(&raw).map_err(|source| StateError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let io_err = |source| StateError::Io {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|source| StateError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        std::fs::write(path, json).map_err(io_err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(io_err)?;
        }
        Ok(())
    }
}

/// Where state lives when the operator does not say.
///
/// `/var/lib/portal-agent` on Linux, which is where a service's data belongs
/// and which a Docker volume can be mounted over.
pub fn default_state_path() -> PathBuf {
    if let Ok(dir) = std::env::var("PORTAL_AGENT_DIR") {
        return PathBuf::from(dir).join("agent.json");
    }
    #[cfg(unix)]
    {
        PathBuf::from("/var/lib/portal-agent/agent.json")
    }
    #[cfg(not(unix))]
    {
        std::env::var("APPDATA")
            .map(|base| PathBuf::from(base).join("portal-agent").join("agent.json"))
            .unwrap_or_else(|_| PathBuf::from("agent.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_proto::wg::generate_keypair;
    use std::net::Ipv4Addr;

    fn state() -> AgentState {
        AgentState {
            gateway_url: "https://portal.example.com".into(),
            agent_id: Uuid::new_v4(),
            agent_key: "secret-agent-key".into(),
            private_key: generate_keypair().private,
            tunnel: TunnelConfig {
                gateway_public_key: generate_keypair().public,
                gateway_endpoint: "vps.example.com:51820".into(),
                tunnel_ip: Ipv4Addr::new(10, 99, 0, 2),
                tunnel_prefix_len: 24,
                persistent_keepalive: 25,
            },
        }
    }

    fn temp_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("portal-agent-{}", Uuid::new_v4()))
            .join("agent.json")
    }

    #[test]
    fn state_survives_a_round_trip() {
        let path = temp_path();
        let original = state();
        original.save(&path).unwrap();

        let loaded = AgentState::load(&path).unwrap();
        assert_eq!(loaded.agent_id, original.agent_id);
        assert_eq!(loaded.agent_key, original.agent_key);
        assert_eq!(loaded.private_key, original.private_key);
        assert_eq!(loaded.tunnel.tunnel_ip, original.tunnel.tunnel_ip);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_state_file_says_how_to_fix_it() {
        let err = AgentState::load(Path::new("/nonexistent/agent.json")).unwrap_err();
        assert!(err.to_string().contains("portal-agent enroll"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_state_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = temp_path();
        state().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_state_names_the_file() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let err = AgentState::load(&path).unwrap_err();
        assert!(matches!(err, StateError::Parse { .. }));
        assert!(err.to_string().contains("agent.json"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
