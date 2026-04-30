use anyhow::{anyhow, Result};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Claims {
    /// host_id issued by Laravel at enrollment.
    sub: String,
    /// Capabilities this host is authorized for. Always includes "coold";
    /// hosts running builds also carry "builder".
    #[serde(default)]
    caps: Vec<String>,
}

pub struct VerifiedJwt {
    pub host_id: String,
    pub caps: Vec<String>,
}

/// Verify a per-host JWT. Audience is fixed to "coold" — capability-based
/// authorization (e.g. accepting a build dispatch) is decided from the
/// `caps` claim, not from audience splits.
pub fn verify_jwt(token: &str, public_key_pem: &str) -> Result<VerifiedJwt> {
    let key = DecodingKey::from_ec_pem(public_key_pem.as_bytes())
        .or_else(|_| DecodingKey::from_rsa_pem(public_key_pem.as_bytes()))
        .map_err(|e| anyhow!("load JWT pubkey: {e}"))?;

    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&["coold"]);

    let data =
        decode::<Claims>(token, &key, &validation).map_err(|e| anyhow!("JWT verification failed: {e}"))?;

    Ok(VerifiedJwt {
        host_id: data.claims.sub,
        caps: data.claims.caps,
    })
}
