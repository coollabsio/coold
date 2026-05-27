use anyhow::{Context, Result};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
struct Claims<'a> {
    sub: &'a str,
    aud: &'a str,
    caps: &'a [String],
    iat: u64,
    exp: u64,
}

pub fn mint_host_jwt(priv_key_pem: &[u8], host_id: &str, caps: &[String]) -> Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let default_caps = vec!["coold".to_string()];
    let caps = if caps.is_empty() { &default_caps } else { caps };
    let claims = Claims {
        sub: host_id,
        aud: "coold",
        caps,
        iat: now,
        exp: now + 365 * 24 * 60 * 60,
    };
    let key = EncodingKey::from_ec_pem(priv_key_pem).context("parse EC private key")?;
    jsonwebtoken::encode(&Header::new(Algorithm::ES256), &claims, &key).context("sign host JWT")
}
