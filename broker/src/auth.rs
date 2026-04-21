use anyhow::{anyhow, Result};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Claims {
    /// host_id issued by Laravel at enrollment.
    sub: String,
}

/// Verify a per-host JWT against the expected audience ("coold" or "builder").
/// Returns the `sub` claim (caller id) on success.
pub fn verify_jwt(token: &str, public_key_pem: &str, expected_audience: &str) -> Result<String> {
    let key = DecodingKey::from_ec_pem(public_key_pem.as_bytes())
        .or_else(|_| DecodingKey::from_rsa_pem(public_key_pem.as_bytes()))
        .map_err(|e| anyhow!("load JWT pubkey: {e}"))?;

    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&[expected_audience]);

    let data = decode::<Claims>(token, &key, &validation)
        .map_err(|e| anyhow!("JWT verification failed: {e}"))?;

    Ok(data.claims.sub)
}
