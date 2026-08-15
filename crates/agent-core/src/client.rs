//! Talking to the gateway.

use portal_proto::api::{AgentAssignment, ApiError, RegisterRequest, RegisterResponse};
use portal_proto::wg::PublicKey;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not reach the gateway at {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("the gateway does not recognise this key; copy it again from the control panel")]
    Unauthorized,
    #[error("gateway returned {status}: {message}")]
    Api { status: u16, message: String },
}

type Result<T> = std::result::Result<T, ClientError>;

pub struct GatewayClient {
    http: reqwest::Client,
    base_url: String,
    key: String,
}

impl GatewayClient {
    pub fn new(base_url: &str, key: &str) -> Result<Self> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            // Shorter than the poll interval, so a wedged connection cannot
            // stall the loop past the next scheduled attempt.
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|source| ClientError::Transport {
                url: base_url.clone(),
                source,
            })?;
        Ok(Self {
            http,
            base_url,
            key: key.trim().to_string(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Announce this agent's freshly generated tunnel identity.
    pub async fn register(&self, public_key: PublicKey) -> Result<RegisterResponse> {
        let url = format!("{}/api/register", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.key)
            .json(&RegisterRequest { public_key })
            .send()
            .await
            .map_err(|source| ClientError::Transport {
                url: url.clone(),
                source,
            })?;
        self.decode(response).await
    }

    /// Fetch the full set of forwards this node should be serving.
    pub async fn assignment(&self) -> Result<AgentAssignment> {
        let url = format!("{}/api/assignment", self.base_url);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(|source| ClientError::Transport {
                url: url.clone(),
                source,
            })?;
        self.decode(response).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T> {
        let status = response.status();
        let url = response.url().to_string();
        if status.is_success() {
            return response
                .json()
                .await
                .map_err(|source| ClientError::Transport { url, source });
        }
        if status.as_u16() == 401 {
            return Err(ClientError::Unauthorized);
        }
        // The gateway's own error text is far more useful than a status code.
        let message = response
            .json::<ApiError>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|_| status.to_string());
        Err(ClientError::Api {
            status: status.as_u16(),
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slashes_do_not_produce_double_slashed_urls() {
        let client = GatewayClient::new("https://portal.example.com/", " key ").unwrap();
        assert_eq!(client.base_url(), "https://portal.example.com");
        assert_eq!(client.key, "key", "a pasted key often brings whitespace");
    }
}
