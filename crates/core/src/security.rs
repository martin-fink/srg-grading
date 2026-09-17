//! Random credentials, constant-time comparisons, and webhook verification.
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

pub fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

pub fn pkce(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn verify_webhook(secret: &[u8], signature: &str, body: &[u8]) -> Result<()> {
    ensure!(
        secret.len() >= 32,
        "webhook secret must contain at least 32 bytes"
    );
    let signature = signature
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow::anyhow!("invalid signature"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
    mac.update(body);
    mac.verify_slice(&hex::decode(signature)?)?;
    Ok(())
}

pub fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_bytes_cannot_be_changed() {
        let key = [42; 32];
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        mac.update(b"payload");
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_webhook(&key, &signature, b"payload").is_ok());
        assert!(verify_webhook(&key, &signature, b"forged").is_err());
    }
}
