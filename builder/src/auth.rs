use anyhow::{anyhow, Result};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct Claims {
    sub: String,
    aud: String,
    iat: u64,
}

/// Sign a JWT with the builder's private key to present on Hello.
pub fn sign_jwt(builder_id: &str, private_key_pem: &str) -> Result<String> {
    let key = EncodingKey::from_ec_pem(private_key_pem.as_bytes())
        .or_else(|_| EncodingKey::from_rsa_pem(private_key_pem.as_bytes()))
        .map_err(|e| anyhow!("load JWT privkey: {e}"))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let claims = Claims {
        sub: builder_id.to_owned(),
        aud: "builder".to_owned(),
        iat: now,
    };

    encode(&Header::new(Algorithm::ES256), &claims, &key)
        .map_err(|e| anyhow!("sign JWT: {e}"))
}
