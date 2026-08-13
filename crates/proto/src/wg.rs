//! WireGuard key handling.
//!
//! Keys are Curve25519 and are exchanged as standard base64, matching what
//! `wg` prints, so anything here can be pasted into a hand-written config for
//! debugging.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;
const KEY_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("key is not valid base64: {0}")]
    Encoding(#[from] base64::DecodeError),
    #[error("key must decode to {KEY_LEN} bytes, got {0}")]
    Length(usize),
}

/// A base64 WireGuard public key, validated on construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublicKey(String);

impl PublicKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PublicKey {
    type Error = KeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let decoded = B64.decode(&value)?;
        if decoded.len() != KEY_LEN {
            return Err(KeyError::Length(decoded.len()));
        }
        Ok(Self(value))
    }
}

impl From<PublicKey> for String {
    fn from(key: PublicKey) -> Self {
        key.0
    }
}

impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A freshly generated keypair. The private half is only ever held by the side
/// that generated it — the agent generates its own and sends up the public key
/// during enrollment.
pub struct KeyPair {
    pub private: String,
    pub public: PublicKey,
}

pub fn generate_keypair() -> KeyPair {
    let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
    let public = x25519_dalek::PublicKey::from(&secret);
    KeyPair {
        private: B64.encode(secret.to_bytes()),
        public: PublicKey(B64.encode(public.to_bytes())),
    }
}

/// Derive the public key for an existing private key, so the agent can show
/// its identity without storing both halves.
pub fn public_from_private(private_b64: &str) -> Result<PublicKey, KeyError> {
    let bytes = B64.decode(private_b64)?;
    let bytes: [u8; KEY_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| KeyError::Length(bytes.len()))?;
    let secret = x25519_dalek::StaticSecret::from(bytes);
    Ok(PublicKey(
        B64.encode(x25519_dalek::PublicKey::from(&secret).to_bytes()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_round_trip() {
        let pair = generate_keypair();
        assert_eq!(public_from_private(&pair.private).unwrap(), pair.public);
    }

    #[test]
    fn public_keys_are_44_char_base64() {
        let pair = generate_keypair();
        assert_eq!(pair.public.as_str().len(), 44);
        assert!(pair.public.as_str().ends_with('='));
    }

    #[test]
    fn rejects_wrong_length_keys() {
        let short = B64.encode([0u8; 16]);
        assert!(matches!(
            PublicKey::try_from(short),
            Err(KeyError::Length(16))
        ));
    }

    #[test]
    fn rejects_non_base64() {
        assert!(matches!(
            PublicKey::try_from("not a key!!".to_string()),
            Err(KeyError::Encoding(_))
        ));
    }
}
