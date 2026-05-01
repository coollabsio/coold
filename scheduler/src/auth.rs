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

enum KeyKind {
    Ec,
    Rsa,
}

/// Verify a per-host JWT. Audience is fixed to "coold" — capability-based
/// authorization (e.g. accepting a build dispatch) is decided from the
/// `caps` claim, not from audience splits.
///
/// The signing algorithm is read from the token header and validated against
/// the loaded key type. HMAC (`HS*`), `EdDSA`, and `none` are always rejected
/// — accepting `HS*` against an asymmetric public key would let an attacker
/// forge tokens by HMAC'ing the public key bytes.
pub fn verify_jwt(token: &str, public_key_pem: &str) -> Result<VerifiedJwt> {
    let (key, kind) = if let Ok(k) = DecodingKey::from_ec_pem(public_key_pem.as_bytes()) {
        (k, KeyKind::Ec)
    } else {
        let k = DecodingKey::from_rsa_pem(public_key_pem.as_bytes())
            .map_err(|e| anyhow!("load JWT pubkey: {e}"))?;
        (k, KeyKind::Rsa)
    };

    let header = decode_header(token).map_err(|e| anyhow!("JWT header parse: {e}"))?;
    let alg = header.alg;
    let allowed = match kind {
        KeyKind::Ec => matches!(alg, Algorithm::ES256 | Algorithm::ES384),
        KeyKind::Rsa => matches!(
            alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
        ),
    };
    if !allowed {
        return Err(anyhow!("JWT alg {alg:?} not allowed for loaded key"));
    }

    let mut validation = Validation::new(alg);
    validation.set_audience(&["coold"]);

    let data =
        decode::<Claims>(token, &key, &validation).map_err(|e| anyhow!("JWT verification failed: {e}"))?;

    Ok(VerifiedJwt {
        host_id: data.claims.sub,
        caps: data.claims.caps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Serialize)]
    struct TestClaims {
        sub: String,
        aud: String,
        caps: Vec<String>,
        exp: usize,
        iat: usize,
    }

    fn now() -> usize {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as usize
    }

    fn claims(exp_offset: i64) -> TestClaims {
        let n = now();
        TestClaims {
            sub: "host-a".into(),
            aud: "coold".into(),
            caps: vec!["coold".into()],
            exp: (n as i64 + exp_offset) as usize,
            iat: n,
        }
    }

    struct EcKeys {
        priv_pkcs8_pem: Vec<u8>,
        pub_pem: Vec<u8>,
        _dir: tempfile::TempDir,
    }

    fn gen_ec_keys() -> EcKeys {
        let dir = tempfile::tempdir().unwrap();
        let priv_path = dir.path().join("ec.pem");
        let pkcs8_path = dir.path().join("ec.pkcs8.pem");
        let pub_path = dir.path().join("ec.pub.pem");
        assert!(Command::new("openssl")
            .args([
                "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out",
                priv_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("openssl")
            .args([
                "pkcs8", "-topk8", "-nocrypt", "-in", priv_path.to_str().unwrap(),
                "-out", pkcs8_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("openssl")
            .args([
                "ec", "-in", priv_path.to_str().unwrap(), "-pubout", "-out",
                pub_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        EcKeys {
            priv_pkcs8_pem: std::fs::read(&pkcs8_path).unwrap(),
            pub_pem: std::fs::read(&pub_path).unwrap(),
            _dir: dir,
        }
    }

    struct RsaKeys {
        priv_pem: Vec<u8>,
        pub_pem: Vec<u8>,
        _dir: tempfile::TempDir,
    }

    fn gen_rsa_keys() -> RsaKeys {
        let dir = tempfile::tempdir().unwrap();
        let priv_path = dir.path().join("rsa.pem");
        let pub_path = dir.path().join("rsa.pub.pem");
        assert!(Command::new("openssl")
            .args([
                "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048",
                "-out", priv_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("openssl")
            .args([
                "rsa", "-in", priv_path.to_str().unwrap(), "-pubout", "-out",
                pub_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        RsaKeys {
            priv_pem: std::fs::read(&priv_path).unwrap(),
            pub_pem: std::fs::read(&pub_path).unwrap(),
            _dir: dir,
        }
    }

    #[test]
    fn es256_accept() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(3600), &enc).unwrap();
        let v = verify_jwt(&jwt, std::str::from_utf8(&keys.pub_pem).unwrap()).unwrap();
        assert_eq!(v.host_id, "host-a");
        assert_eq!(v.caps, vec!["coold".to_string()]);
    }

    #[test]
    fn rs256_accept() {
        let keys = gen_rsa_keys();
        let enc = EncodingKey::from_rsa_pem(&keys.priv_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::RS256), &claims(3600), &enc).unwrap();
        let v = verify_jwt(&jwt, std::str::from_utf8(&keys.pub_pem).unwrap()).unwrap();
        assert_eq!(v.host_id, "host-a");
    }

    #[test]
    fn hs256_rejected_against_asymmetric_key() {
        let keys = gen_ec_keys();
        // Forge HS256 token using public key bytes as HMAC secret — classic
        // key-confusion attack. Must be rejected purely on alg, before signature check.
        let enc = EncodingKey::from_secret(&keys.pub_pem);
        let jwt = encode(&Header::new(Algorithm::HS256), &claims(3600), &enc).unwrap();
        let err = verify_jwt(&jwt, std::str::from_utf8(&keys.pub_pem).unwrap()).err().unwrap();
        assert!(err.to_string().contains("not allowed"), "got: {err}");
    }

    #[test]
    fn alg_key_mismatch_rejected() {
        let ec = gen_ec_keys();
        let rsa = gen_rsa_keys();
        // ES256 token verified against RSA public key — must be rejected on alg.
        let enc = EncodingKey::from_ec_pem(&ec.priv_pkcs8_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(3600), &enc).unwrap();
        let err = verify_jwt(&jwt, std::str::from_utf8(&rsa.pub_pem).unwrap()).err().unwrap();
        assert!(err.to_string().contains("not allowed"), "got: {err}");
    }

    #[test]
    fn expired_rejected() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(-3600), &enc).unwrap();
        let err = verify_jwt(&jwt, std::str::from_utf8(&keys.pub_pem).unwrap()).err().unwrap();
        assert!(err.to_string().contains("verification failed"), "got: {err}");
    }
}
