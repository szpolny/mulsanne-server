//! Request signing. Each install has an Ed25519 key; every request carries
//! the public key, a Unix timestamp and a signature over
//! `METHOD \n path?query \n timestamp \n hex(sha256(body))`.
//! The server rejects timestamps more than [`MAX_CLOCK_SKEW_SECS`] off.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub const HEADER_KEY: &str = "x-mulsanne-key";
pub const HEADER_TIMESTAMP: &str = "x-mulsanne-timestamp";
pub const HEADER_SIGNATURE: &str = "x-mulsanne-signature";
pub const MAX_CLOCK_SKEW_SECS: i64 = 60;

pub fn message(method: &str, path_and_query: &str, timestamp: i64, body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}\n{}\n{}\n{}", method.to_ascii_uppercase(), path_and_query, timestamp, hex)
}

/// Header values for a signed request: (key, timestamp, signature).
pub fn sign(
    key: &SigningKey,
    method: &str,
    path_and_query: &str,
    timestamp: i64,
    body: &[u8],
) -> (String, String, String) {
    let sig = key.sign(message(method, path_and_query, timestamp, body).as_bytes());
    (
        URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
        timestamp.to_string(),
        URL_SAFE_NO_PAD.encode(sig.to_bytes()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    Malformed,
    Expired,
    BadSignature,
}

impl VerifyError {
    pub fn message(self) -> &'static str {
        match self {
            VerifyError::Malformed => "Missing or malformed signature headers",
            VerifyError::Expired => "Request timestamp is too far from server time; check your PC clock",
            VerifyError::BadSignature => "Invalid request signature",
        }
    }
}

/// Checks a signed request and returns the signer's public key.
pub fn verify(
    key: &str,
    timestamp: &str,
    signature: &str,
    method: &str,
    path_and_query: &str,
    body: &[u8],
    now: i64,
) -> Result<VerifyingKey, VerifyError> {
    let key_bytes: [u8; 32] = decode_fixed(key)?;
    let sig_bytes: [u8; 64] = decode_fixed(signature)?;
    let timestamp: i64 = timestamp.parse().map_err(|_| VerifyError::Malformed)?;
    if (now - timestamp).abs() > MAX_CLOCK_SKEW_SECS {
        return Err(VerifyError::Expired);
    }
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| VerifyError::Malformed)?;
    let msg = message(method, path_and_query, timestamp, body);
    key.verify(msg.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| VerifyError::BadSignature)?;
    Ok(key)
}

fn decode_fixed<const N: usize>(value: &str) -> Result<[u8; N], VerifyError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or(VerifyError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;
    const BODY: &[u8] = br#"{"display_name":"Test"}"#;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    #[test]
    fn round_trip() {
        let (k, t, s) = sign(&key(), "post", "/v1/users", NOW, BODY);
        let verified = verify(&k, &t, &s, "POST", "/v1/users", BODY, NOW + 30).unwrap();
        assert_eq!(verified, key().verifying_key());
    }

    #[test]
    fn rejects_tampering_and_stale_requests() {
        let (k, t, s) = sign(&key(), "POST", "/v1/users", NOW, BODY);
        assert_eq!(verify(&k, &t, &s, "POST", "/v1/users", b"{}", NOW), Err(VerifyError::BadSignature));
        assert_eq!(verify(&k, &t, &s, "DELETE", "/v1/users", BODY, NOW), Err(VerifyError::BadSignature));
        assert_eq!(verify(&k, &t, &s, "POST", "/v1/me", BODY, NOW), Err(VerifyError::BadSignature));
        assert_eq!(verify(&k, &t, &s, "POST", "/v1/users", BODY, NOW + 61), Err(VerifyError::Expired));
        assert_eq!(verify(&k, &t, "abc", "POST", "/v1/users", BODY, NOW), Err(VerifyError::Malformed));
        assert_eq!(verify(&k, "soon", &s, "POST", "/v1/users", BODY, NOW), Err(VerifyError::Malformed));
    }

    /// Same vector as `online::tests::matches_server_vector` in the app.
    #[test]
    fn matches_app_vector() {
        let (k, t, s) = sign(&key(), "POST", "/v1/users", NOW, BODY);
        assert_eq!(k, "6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw");
        assert_eq!(t, "1700000000");
        assert_eq!(s, "0QCsbQqDOdai67gF5wVmmMC0_l_wVweKB6A8Uhp82USV2RCAXWABI-wQBIX9c9m4uE4RIxTceZb0xEbUI54sBw");
    }
}
