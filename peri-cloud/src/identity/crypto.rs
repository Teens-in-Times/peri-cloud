use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{IdentityError, IdentityResult};

pub(crate) fn secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub(crate) fn hash(domain: &str, input: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.as_bytes());
    digest.update([0]);
    digest.update(input.as_bytes());
    format!("{:x}", digest.finalize())
}

pub(crate) fn equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

pub(crate) async fn password_hash(
    password: String,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> IdentityResult<String> {
    tokio::task::spawn_blocking(move || {
        // The blocking KDF owns its admission permit even if HTTP is aborted.
        let _permit = permit;
        let salt =
            SaltString::encode_b64(Uuid::new_v4().as_bytes()).map_err(|_| IdentityError::Crypto)?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| IdentityError::Crypto)
    })
    .await
    .map_err(|_| IdentityError::Crypto)?
}

pub(crate) async fn verify_password(
    password: String,
    encoded: String,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> IdentityResult<bool> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let hash = PasswordHash::new(&encoded).map_err(|_| IdentityError::Crypto)?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok())
    })
    .await
    .map_err(|_| IdentityError::Crypto)?
}

pub(crate) fn challenge(verifier: &str) -> IdentityResult<String> {
    if !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
    {
        return Err(IdentityError::Invalid);
    }
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
