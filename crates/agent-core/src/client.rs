//! Talking to the gateway.

use portal_proto::api::{AgentAssignment, ApiError, EnrollRequest, EnrollResponse};
use portal_proto::wg::PublicKey;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not reach the gateway: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("the gateway rejected the enrollment token; mint a new one in the web UI")]
    BadToken,
    #[error("the gateway no longer recognises this agent; re-enroll it")]
    Unauthorized,
    #[error("gateway returned {status}: {message}")]
    Api { status: u16, message: String },
}

type Result<T> = std::result::Result<T, ClientError>;

pub struct GatewayClient {
    http: reqwest::Client,
    base_url: String,
    agent_key: Option<String>,
}

impl GatewayClient {
    pub fn new(base_url: impl Into<String>, agent_key: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Shorter than the poll interval, so a wedged connection cannot
            // stall the loop past the next scheduled attempt.
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            agent_key,
        })
    }

    /// Trade a one-time enrollment token for a place in the tunnel.
    pub async fn enroll(
        &self,
        token: &str,
        name: &str,
        public_key: PublicKey,
    ) -> Result<EnrollResponse> {
        let response = self
            .http
            .post(format!("{}/api/enroll", self.base_url))
            .json(&EnrollRequest {
                token: token.to_string(),
                name: name.to_string(),
                public_key,
            })
            .send()
            .await?;
        self.decode(response).await
    }

    /// Fetch the full set of forwards this agent should be serving.
    pub async fn assignment(&self) -> Result<AgentAssignment> {
        let key = self.agent_key.as_deref().unwrap_or_default();
        let response = self
            .http
            .get(format!("{}/api/assignment", self.base_url))
            .bearer_auth(key)
            .send()
            .await?;
        self.decode(response).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T> {
        let status = response.status();
        if status.is_success() {
            return Ok(response.json().await?);
        }
        // The gateway's own error text is far more useful than a status code —
        // "no profile named `minecraft-jvaa`" beats "400 Bad Request".
        let message = response
            .json::<ApiError>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|_| status.to_string());
        Err(match status.as_u16() {
            401 => ClientError::Unauthorized,
            403 => ClientError::BadToken,
            other => ClientError::Api {
                status: other,
                message,
            },
        })
    }
}
