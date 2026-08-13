//! Cloudflare DNS client.
//!
//! Cloudflare is a control plane here, not a data plane — see the README. The
//! token needs `Zone:DNS:Edit` on one zone and nothing else; it cannot be
//! scoped tighter than a zone, which is exactly why reconciliation refuses to
//! touch names outside the service it was given.
//!
//! Every record written is `proxied: false`. The orange cloud will not carry
//! TCP 25565, so proxying a game record does not hide the origin, it breaks
//! the game.

use crate::dns::{DnsPlan, DnsRecord, ExistingRecord};
use serde::Deserialize;
use std::time::Duration;

const API_BASE: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, thiserror::Error)]
pub enum CloudflareError {
    #[error("cloudflare request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("cloudflare rejected the request: {0}")]
    Api(String),
    #[error("cloudflare returned a record this gateway does not understand: {0}")]
    UnknownRecord(String),
}

type Result<T> = std::result::Result<T, CloudflareError>;

pub struct Cloudflare {
    http: reqwest::Client,
    zone_id: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiMessage>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct RecordJson {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    ttl: u32,
    #[serde(default)]
    data: Option<SrvData>,
}

#[derive(Debug, Deserialize)]
struct SrvData {
    #[serde(default)]
    priority: u16,
    #[serde(default)]
    weight: u16,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    target: String,
}

impl Cloudflare {
    pub fn new(zone_id: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            // A hung DNS call must not wedge service creation; the reconciler
            // is declarative and will simply try again.
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            zone_id: zone_id.into(),
            token: token.into(),
        })
    }

    /// Every record in the zone, as this gateway models them.
    ///
    /// Types the gateway does not manage (MX, TXT, CNAME…) are skipped rather
    /// than reported: they belong to the operator, and reconciliation must be
    /// able to see past them without opinions.
    pub async fn list_records(&self) -> Result<Vec<ExistingRecord>> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{API_BASE}/zones/{}/dns_records?per_page=100&page={page}",
                self.zone_id
            );
            let body: ApiResponse<Vec<RecordJson>> = self
                .http
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await?
                .json()
                .await?;
            let records = self.unwrap_response(body)?;
            let count = records.len();
            for r in records {
                if let Some(record) = to_dns_record(&r) {
                    out.push(ExistingRecord { id: r.id, record });
                }
            }
            if count < 100 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    pub async fn apply(&self, plan: &DnsPlan) -> Result<()> {
        for record in &plan.create {
            let url = format!("{API_BASE}/zones/{}/dns_records", self.zone_id);
            let body: ApiResponse<serde_json::Value> = self
                .http
                .post(&url)
                .bearer_auth(&self.token)
                .json(&to_payload(record))
                .send()
                .await?
                .json()
                .await?;
            self.unwrap_response(body)?;
        }
        for (id, record) in &plan.update {
            let url = format!("{API_BASE}/zones/{}/dns_records/{id}", self.zone_id);
            let body: ApiResponse<serde_json::Value> = self
                .http
                .put(&url)
                .bearer_auth(&self.token)
                .json(&to_payload(record))
                .send()
                .await?
                .json()
                .await?;
            self.unwrap_response(body)?;
        }
        for id in &plan.delete {
            let url = format!("{API_BASE}/zones/{}/dns_records/{id}", self.zone_id);
            let body: ApiResponse<serde_json::Value> = self
                .http
                .delete(&url)
                .bearer_auth(&self.token)
                .send()
                .await?
                .json()
                .await?;
            self.unwrap_response(body)?;
        }
        Ok(())
    }

    fn unwrap_response<T>(&self, body: ApiResponse<T>) -> Result<T> {
        if !body.success {
            let detail = body
                .errors
                .iter()
                .map(|e| format!("{} ({})", e.message, e.code))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(CloudflareError::Api(if detail.is_empty() {
                "no reason given".to_string()
            } else {
                detail
            }));
        }
        body.result
            .ok_or_else(|| CloudflareError::Api("response had no result".into()))
    }
}

/// Convert a record into Cloudflare's JSON body.
fn to_payload(record: &DnsRecord) -> serde_json::Value {
    match record {
        DnsRecord::A { name, address, ttl } => serde_json::json!({
            "type": "A",
            "name": name,
            "content": address.to_string(),
            "ttl": ttl,
            "proxied": record.proxied(),
        }),
        DnsRecord::Srv {
            name,
            target,
            port,
            priority,
            weight,
            ttl,
        } => serde_json::json!({
            "type": "SRV",
            "name": name,
            "ttl": ttl,
            "data": {
                "priority": priority,
                "weight": weight,
                "port": port,
                "target": target,
            },
        }),
    }
}

fn to_dns_record(json: &RecordJson) -> Option<DnsRecord> {
    match json.kind.as_str() {
        "A" => Some(DnsRecord::A {
            name: json.name.clone(),
            address: json.content.parse().ok()?,
            ttl: json.ttl,
        }),
        "SRV" => {
            let data = json.data.as_ref()?;
            Some(DnsRecord::Srv {
                name: json.name.clone(),
                target: data.target.clone(),
                port: data.port,
                priority: data.priority,
                weight: data.weight,
                ttl: json.ttl,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn an_a_record_payload_is_never_proxied() {
        let payload = to_payload(&DnsRecord::A {
            name: "mc.example.com".into(),
            address: Ipv4Addr::new(203, 0, 113, 10),
            ttl: 120,
        });
        assert_eq!(payload["type"], "A");
        assert_eq!(payload["content"], "203.0.113.10");
        assert_eq!(
            payload["proxied"], false,
            "the orange cloud cannot carry a game port"
        );
    }

    #[test]
    fn an_srv_payload_nests_its_parts_where_cloudflare_wants_them() {
        let payload = to_payload(&DnsRecord::Srv {
            name: "_minecraft._tcp.mc.example.com".into(),
            target: "mc.example.com".into(),
            port: 30000,
            priority: 0,
            weight: 5,
            ttl: 120,
        });
        assert_eq!(payload["type"], "SRV");
        assert_eq!(payload["data"]["port"], 30000);
        assert_eq!(payload["data"]["target"], "mc.example.com");
        assert_eq!(payload["data"]["weight"], 5);
    }

    #[test]
    fn records_round_trip_through_cloudflares_shape() {
        let json = RecordJson {
            id: "abc".into(),
            kind: "A".into(),
            name: "mc.example.com".into(),
            content: "203.0.113.10".into(),
            ttl: 120,
            data: None,
        };
        let record = to_dns_record(&json).expect("A records are understood");
        assert_eq!(to_payload(&record)["content"], "203.0.113.10");
    }

    #[test]
    fn record_types_the_gateway_does_not_manage_are_ignored() {
        for kind in ["MX", "TXT", "CNAME", "AAAA"] {
            let json = RecordJson {
                id: "x".into(),
                kind: kind.into(),
                name: "example.com".into(),
                content: "whatever".into(),
                ttl: 300,
                data: None,
            };
            assert!(
                to_dns_record(&json).is_none(),
                "{kind} belongs to the operator, not the gateway"
            );
        }
    }

    #[test]
    fn an_srv_record_without_data_is_skipped_rather_than_panicking() {
        let json = RecordJson {
            id: "x".into(),
            kind: "SRV".into(),
            name: "_minecraft._tcp.mc.example.com".into(),
            content: String::new(),
            ttl: 120,
            data: None,
        };
        assert!(to_dns_record(&json).is_none());
    }
}
