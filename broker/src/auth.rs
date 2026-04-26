use anyhow::{anyhow, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
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
///
/// The algorithm is inferred from the JWT header (`alg`) so RSA and EC keys
/// are both supported without extra configuration.
pub fn verify_jwt(token: &str, public_key_pem: &str) -> Result<VerifiedJwt> {
    let header = decode_header(token).map_err(|e| anyhow!("decode JWT header: {e}"))?;

    let (key, mut validation) = match header.alg {
        Algorithm::ES256 | Algorithm::ES384 => {
            let key = DecodingKey::from_ec_pem(public_key_pem.as_bytes())
                .map_err(|e| anyhow!("load EC JWT pubkey: {e}"))?;
            (key, Validation::new(header.alg))
        }
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
            let key = DecodingKey::from_rsa_pem(public_key_pem.as_bytes())
                .map_err(|e| anyhow!("load RSA JWT pubkey: {e}"))?;
            (key, Validation::new(header.alg))
        }
        other => return Err(anyhow!("unsupported JWT algorithm: {other:?}")),
    };
    validation.set_audience(&["coold"]);

    let data =
        decode::<Claims>(token, &key, &validation).map_err(|e| anyhow!("JWT verification failed: {e}"))?;

    Ok(VerifiedJwt {
        host_id: data.claims.sub,
        caps: data.claims.caps,
    })
}
