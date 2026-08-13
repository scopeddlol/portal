//! Bearer tokens: enrollment tokens, agent keys, the admin login.
//!
//! All three are the same shape — 256 bits of randomness, URL-safe base64 —
//! and all three are stored as a SHA-256 hash. Someone who gets a copy of
//! `portal.db` (a backup, a stolen disk image) then holds no usable
//! credentials, which matters because an agent key can publish DNS and an
//! admin token can do anything at all.

use base64::Engine as _;
use sha2::{Digest, Sha256};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A fresh random token, shown to the operator exactly once.
pub fn generate() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    B64.encode(bytes)
}

/// Hex SHA-256 of a token, which is what the database holds.
pub fn hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare a presented token against a stored hash in constant time with
/// respect to the hash contents, so a wrong guess leaks nothing through how
/// long the check took.
pub fn verify(presented: &str, stored_hash: &str) -> bool {
    let computed = hash(presented);
    if computed.len() != stored_hash.len() {
        return false;
    }
    computed
        .bytes()
        .zip(stored_hash.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_never_repeat() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b);
        assert!(a.len() >= 43, "256 bits of base64: {a}");
    }

    #[test]
    fn hashing_is_stable_and_not_the_token_itself() {
        let token = generate();
        assert_eq!(hash(&token), hash(&token));
        assert_ne!(hash(&token), token);
        assert_eq!(hash("").len(), 64);
    }

    #[test]
    fn verify_accepts_the_real_token_and_rejects_everything_else() {
        let token = generate();
        let stored = hash(&token);
        assert!(verify(&token, &stored));
        assert!(!verify("wrong", &stored));
        assert!(!verify(&token, "short"));
    }
}
