use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

/// `kid` used for the single default key when Laravel does not (or cannot)
/// rotate. Laravel now mints `kid=flux-default`; a token with no `kid` at all
/// still falls back to the default key for back-compat.
pub const DEFAULT_KID: &str = "flux-default";

#[derive(Debug, Deserialize)]
struct Claims {
    /// host_id issued by Laravel at enrollment.
    sub: String,
    /// Capabilities this host is authorized for.
    #[serde(default)]
    caps: Vec<String>,
    /// Expiry (seconds since epoch). Present because `Validation` requires it;
    /// captured so the stream loop can enforce a max lifetime (#4).
    exp: u64,
    /// Optional JWT ID used for revocation (#3).
    #[serde(default)]
    jti: Option<String>,
    /// Tenant (team) the host belongs to (#2). Laravel mints this; flux
    /// requires it (behind `COOLIFY_FLUX_REQUIRE_TEAM_ID`) so every stream is
    /// scoped to a tenant and the binding is auditable.
    #[serde(default)]
    team_id: Option<String>,
}

pub struct VerifiedJwt {
    pub host_id: String,
    pub caps: Vec<String>,
    /// Token expiry (seconds since epoch). The stream loop drops the stream at
    /// this instant so a long-lived stream re-authenticates (#4).
    pub exp: u64,
    /// Tenant claim (#2). `None`/blank when Laravel did not mint one — enforced
    /// by [`team_id_satisfied`] at connect.
    pub team_id: Option<String>,
}

/// Consulted by [`verify_jwt`] to reject revoked tokens (#3). Implemented by
/// the revocation store; a trait keeps `auth` decoupled from its storage so
/// both are independently unit-testable.
pub trait RevocationCheck {
    /// Return `true` when the given `jti` is on the denylist.
    fn is_revoked(&self, jti: &str) -> bool;
}

/// A [`RevocationCheck`] that never revokes — used where revocation is not
/// wired (and in tests).
pub struct NoRevocations;

impl RevocationCheck for NoRevocations {
    fn is_revoked(&self, _jti: &str) -> bool {
        false
    }
}

/// Set of JWT verification keys selectable by the token header `kid` (S3).
///
/// A single default key covers the common (unrotated) case; additional keys
/// keyed by `kid` support overlapping rotation windows. Selection is:
/// no `kid` or `kid=flux-default` → default key; any other `kid` → the
/// matching additional key, or reject if unknown.
pub struct JwtKeys {
    default_pem: String,
    by_kid: HashMap<String, String>,
}

impl JwtKeys {
    pub fn new(default_pem: String, by_kid: HashMap<String, String>) -> Self {
        Self {
            default_pem,
            by_kid,
        }
    }

    /// Single-key set (no rotation) — back-compat convenience.
    pub fn single(default_pem: String) -> Self {
        Self {
            default_pem,
            by_kid: HashMap::new(),
        }
    }

    /// Resolve the verification key PEM for a token's `kid` header.
    fn resolve(&self, kid: Option<&str>) -> Result<&str> {
        match kid {
            None => Ok(&self.default_pem),
            Some(k) if k == DEFAULT_KID => Ok(&self.default_pem),
            Some(k) => self
                .by_kid
                .get(k)
                .map(String::as_str)
                .ok_or_else(|| anyhow!("unknown JWT kid {k:?}")),
        }
    }
}

enum KeyKind {
    Ec,
    Rsa,
}

/// Current wall-clock as seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds remaining until `exp` given `now`; `0` if already expired (#4).
pub fn seconds_until_expiry(exp: u64, now: u64) -> u64 {
    exp.saturating_sub(now)
}

/// Whether a token with expiry `exp` is expired at `now` (#4).
pub fn token_expired(exp: u64, now: u64) -> bool {
    now >= exp
}

/// Whether a token's remaining lifetime is within `max_lifetime_secs` (#4).
/// A `max` of `0` disables the clamp (always within).
pub fn lifetime_within_clamp(exp: u64, now: u64, max_lifetime_secs: u64) -> bool {
    if max_lifetime_secs == 0 {
        return true;
    }
    seconds_until_expiry(exp, now) <= max_lifetime_secs
}

/// Verify a per-host JWT. Audience is fixed to "coold" — capability-based
/// authorization (e.g. accepting a build dispatch) is decided from the
/// `caps` claim, not from audience splits.
///
/// The signing algorithm is read from the token header and validated against
/// the resolved key type. HMAC (`HS*`), `EdDSA`, and `none` are always
/// rejected — accepting `HS*` against an asymmetric public key would let an
/// attacker forge tokens by HMAC'ing the public key bytes.
///
/// Additional enforcement:
/// - S3: the header `kid` selects the verification key; unknown `kid` is rejected.
/// - #4: a token whose remaining lifetime (`exp - now`) exceeds
///   `max_lifetime_secs` is rejected at connect (clamp; `0` disables).
/// - #3: a token whose `jti` is on the revocation denylist is rejected.
pub fn verify_jwt(
    token: &str,
    keys: &JwtKeys,
    revocation: &dyn RevocationCheck,
    now: u64,
    max_lifetime_secs: u64,
) -> Result<VerifiedJwt> {
    let header = decode_header(token).map_err(|e| anyhow!("JWT header parse: {e}"))?;

    let public_key_pem = keys.resolve(header.kid.as_deref())?;
    let (key, kind) = if let Ok(k) = DecodingKey::from_ec_pem(public_key_pem.as_bytes()) {
        (k, KeyKind::Ec)
    } else {
        let k = DecodingKey::from_rsa_pem(public_key_pem.as_bytes())
            .map_err(|e| anyhow!("load JWT pubkey: {e}"))?;
        (k, KeyKind::Rsa)
    };

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
    // jsonwebtoken 9.x defaults `leeway` to 60s — enough slack for normal NTP
    // drift between Laravel and the flux. Reassert explicitly so a future
    // major-version bump that changes the default surfaces here, not at the
    // expiry boundary.
    validation.leeway = 60;

    let data = decode::<Claims>(token, &key, &validation)
        .map_err(|e| anyhow!("JWT verification failed: {e}"))?;
    let claims = data.claims;

    // #4: reject over-clamp lifetimes at connect. A short-lived stream forces
    // periodic re-auth; the stream loop additionally drops the stream at `exp`.
    if !lifetime_within_clamp(claims.exp, now, max_lifetime_secs) {
        return Err(anyhow!(
            "JWT remaining lifetime {}s exceeds max {}s",
            seconds_until_expiry(claims.exp, now),
            max_lifetime_secs
        ));
    }

    // #3: reject revoked tokens.
    if let Some(jti) = claims.jti.as_deref() {
        if revocation.is_revoked(jti) {
            return Err(anyhow!("JWT jti {jti} revoked"));
        }
    }

    Ok(VerifiedJwt {
        host_id: claims.sub,
        caps: claims.caps,
        exp: claims.exp,
        team_id: claims.team_id,
    })
}

/// Whether a token's `team_id` claim satisfies the tenant-binding requirement
/// (#2). An absent or blank `team_id` is rejected when `required`; when
/// enforcement is off (`COOLIFY_FLUX_REQUIRE_TEAM_ID=0`) any value (incl. none)
/// is accepted so legacy tokens can be tolerated during dev.
pub fn team_id_satisfied(team_id: Option<&str>, required: bool) -> bool {
    if !required {
        return true;
    }
    team_id.is_some_and(|team| !team.trim().is_empty())
}

/// Outcome of the per-connection host-binding decision (#2).
#[derive(Debug, PartialEq, Eq)]
pub enum HostBinding {
    /// The token `sub` was verified against the transport peer IP — the
    /// strongest signal: a token replayed from a different host IP is caught.
    Transport,
    /// Transport-level binding was unavailable (`reason`); the token `sub` was
    /// instead verified against the Hello-advertised `host_mgmt_ip`. Weaker —
    /// the advertised IP is self-asserted — so the caller should log a warning.
    HelloFallback { reason: &'static str },
    /// Binding could not be established but enforcement is disabled
    /// (`COOLIFY_FLUX_REQUIRE_HOST_BINDING=0`); the stream is allowed and the
    /// caller should log a warning carrying `detail`.
    Unenforced { detail: String },
}

/// Bind a presented token to the host presenting it (#2).
///
/// Signals, strongest first:
/// 1. `peer_ip` — the transport peer address tonic observed for the gRPC
///    connection. Over the WireGuard mesh this is the host's mgmt IP, which by
///    design equals the token `sub` (host id). A stolen token replayed from a
///    different host IP fails here — this is the strongest available signal.
/// 2. `advertised_mgmt_ip` — the `host_mgmt_ip` coold sends in Hello. This is
///    self-asserted by the connecting party (an attacker replaying a token
///    controls it too), so it only catches inconsistency/misconfiguration, not
///    replay. Used only when the transport peer address is unavailable.
///
/// Returns `Err(reason)` when binding fails and `require` is true (the caller
/// must reject the stream); otherwise `Ok(HostBinding)` describing how the
/// binding was established (for logging).
pub fn decide_host_binding(
    token_sub: &str,
    advertised_mgmt_ip: &str,
    peer_ip: Option<IpAddr>,
    require: bool,
) -> Result<HostBinding, String> {
    match peer_ip {
        Some(peer) => match token_sub.parse::<IpAddr>() {
            Ok(sub_ip) if peer == sub_ip => Ok(HostBinding::Transport),
            Ok(sub_ip) => reject_or_unenforced(
                require,
                format!("transport peer {peer} does not match token sub {sub_ip}"),
            ),
            // `sub` is not an IP (a non-mesh / UUID-id deployment): the
            // transport signal can't be compared, so degrade to the Hello
            // check rather than hard-fail and break the mesh.
            Err(_) => bind_via_hello(
                token_sub,
                advertised_mgmt_ip,
                require,
                "token sub is not an IP address; transport binding not applicable",
            ),
        },
        None => bind_via_hello(
            token_sub,
            advertised_mgmt_ip,
            require,
            "transport peer address unavailable",
        ),
    }
}

/// Fallback host-binding path: verify the token `sub` against the self-asserted
/// Hello `host_mgmt_ip`. `reason` explains why the stronger transport signal
/// was not used (logged by the caller).
fn bind_via_hello(
    token_sub: &str,
    advertised_mgmt_ip: &str,
    require: bool,
    reason: &'static str,
) -> Result<HostBinding, String> {
    if !token_sub.is_empty() && advertised_mgmt_ip == token_sub {
        Ok(HostBinding::HelloFallback { reason })
    } else {
        reject_or_unenforced(
            require,
            format!(
                "{reason}; Hello-advertised host_mgmt_ip {advertised_mgmt_ip:?} \
                 does not match token sub {token_sub:?}"
            ),
        )
    }
}

fn reject_or_unenforced(require: bool, detail: String) -> Result<HostBinding, String> {
    if require {
        Err(detail)
    } else {
        Ok(HostBinding::Unenforced { detail })
    }
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
        #[serde(skip_serializing_if = "Option::is_none")]
        jti: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
    }

    fn now() -> usize {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize
    }

    fn claims(exp_offset: i64) -> TestClaims {
        let n = now();
        TestClaims {
            sub: "host-a".into(),
            aud: "coold".into(),
            caps: vec!["containers.list".into()],
            exp: (n as i64 + exp_offset) as usize,
            iat: n,
            jti: None,
            team_id: Some("team-1".into()),
        }
    }

    /// Revocation checker backed by a fixed set of revoked jtis.
    struct RevokedSet(std::collections::HashSet<String>);

    impl RevocationCheck for RevokedSet {
        fn is_revoked(&self, jti: &str) -> bool {
            self.0.contains(jti)
        }
    }

    fn revoked(jtis: &[&str]) -> RevokedSet {
        RevokedSet(jtis.iter().map(|s| s.to_string()).collect())
    }

    /// Verify against a single default key with the clamp disabled and no
    /// revocations — the common shape for the alg/kid tests.
    fn verify_default(jwt: &str, pub_pem: &[u8]) -> Result<VerifiedJwt> {
        let keys = JwtKeys::single(std::str::from_utf8(pub_pem).unwrap().to_string());
        verify_jwt(jwt, &keys, &NoRevocations, now() as u64, 0)
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
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
                priv_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("openssl")
            .args([
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                priv_path.to_str().unwrap(),
                "-out",
                pkcs8_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("openssl")
            .args([
                "ec",
                "-in",
                priv_path.to_str().unwrap(),
                "-pubout",
                "-out",
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
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out",
                priv_path.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("openssl")
            .args([
                "rsa",
                "-in",
                priv_path.to_str().unwrap(),
                "-pubout",
                "-out",
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
        let v = verify_default(&jwt, &keys.pub_pem).unwrap();
        assert_eq!(v.host_id, "host-a");
        assert_eq!(v.caps, vec!["containers.list".to_string()]);
    }

    #[test]
    fn rs256_accept() {
        let keys = gen_rsa_keys();
        let enc = EncodingKey::from_rsa_pem(&keys.priv_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::RS256), &claims(3600), &enc).unwrap();
        let v = verify_default(&jwt, &keys.pub_pem).unwrap();
        assert_eq!(v.host_id, "host-a");
    }

    #[test]
    fn hs256_rejected_against_asymmetric_key() {
        let keys = gen_ec_keys();
        // Forge HS256 token using public key bytes as HMAC secret — classic
        // key-confusion attack. Must be rejected purely on alg, before signature check.
        let enc = EncodingKey::from_secret(&keys.pub_pem);
        let jwt = encode(&Header::new(Algorithm::HS256), &claims(3600), &enc).unwrap();
        let err = verify_default(&jwt, &keys.pub_pem).err().unwrap();
        assert!(err.to_string().contains("not allowed"), "got: {err}");
    }

    #[test]
    fn alg_key_mismatch_rejected() {
        let ec = gen_ec_keys();
        let rsa = gen_rsa_keys();
        // ES256 token verified against RSA public key — must be rejected on alg.
        let enc = EncodingKey::from_ec_pem(&ec.priv_pkcs8_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(3600), &enc).unwrap();
        let err = verify_default(&jwt, &rsa.pub_pem).err().unwrap();
        assert!(err.to_string().contains("not allowed"), "got: {err}");
    }

    #[test]
    fn expired_rejected() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(-3600), &enc).unwrap();
        let err = verify_default(&jwt, &keys.pub_pem).err().unwrap();
        assert!(
            err.to_string().contains("verification failed"),
            "got: {err}"
        );
    }

    // ── #4: max-lifetime clamp ────────────────────────────────────────────

    #[test]
    fn over_clamp_lifetime_rejected_at_connect() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        // 2h token, clamp 1h → rejected on lifetime, not signature.
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(7200), &enc).unwrap();
        let jwtkeys = JwtKeys::single(std::str::from_utf8(&keys.pub_pem).unwrap().to_string());
        let err = verify_jwt(&jwt, &jwtkeys, &NoRevocations, now() as u64, 3600)
            .err()
            .unwrap();
        assert!(err.to_string().contains("exceeds max"), "got: {err}");
    }

    #[test]
    fn within_clamp_lifetime_accepted() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(1800), &enc).unwrap();
        let jwtkeys = JwtKeys::single(std::str::from_utf8(&keys.pub_pem).unwrap().to_string());
        let v = verify_jwt(&jwt, &jwtkeys, &NoRevocations, now() as u64, 3600).unwrap();
        assert_eq!(v.host_id, "host-a");
    }

    #[test]
    fn expiry_timer_helpers_detect_expiry() {
        assert!(token_expired(100, 100));
        assert!(token_expired(100, 101));
        assert!(!token_expired(100, 99));
        assert_eq!(seconds_until_expiry(150, 100), 50);
        assert_eq!(seconds_until_expiry(100, 150), 0);
        assert!(lifetime_within_clamp(100, 50, 60)); // 50s <= 60s
        assert!(!lifetime_within_clamp(200, 50, 60)); // 150s > 60s
        assert!(lifetime_within_clamp(u64::MAX, 0, 0)); // clamp disabled
    }

    // ── #3: revocation ────────────────────────────────────────────────────

    #[test]
    fn revoked_jti_rejected() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        let mut c = claims(3600);
        c.jti = Some("token-1".into());
        let jwt = encode(&Header::new(Algorithm::ES256), &c, &enc).unwrap();
        let jwtkeys = JwtKeys::single(std::str::from_utf8(&keys.pub_pem).unwrap().to_string());
        let err = verify_jwt(&jwt, &jwtkeys, &revoked(&["token-1"]), now() as u64, 0)
            .err()
            .unwrap();
        assert!(err.to_string().contains("revoked"), "got: {err}");
    }

    #[test]
    fn unrevoked_jti_accepted() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        let mut c = claims(3600);
        c.jti = Some("token-2".into());
        let jwt = encode(&Header::new(Algorithm::ES256), &c, &enc).unwrap();
        let jwtkeys = JwtKeys::single(std::str::from_utf8(&keys.pub_pem).unwrap().to_string());
        let v = verify_jwt(&jwt, &jwtkeys, &revoked(&["other"]), now() as u64, 0).unwrap();
        assert_eq!(v.host_id, "host-a");
    }

    // ── S3: kid / key rotation ────────────────────────────────────────────

    fn sign_with_kid(priv_pkcs8_pem: &[u8], kid: Option<&str>) -> String {
        let enc = EncodingKey::from_ec_pem(priv_pkcs8_pem).unwrap();
        let mut header = Header::new(Algorithm::ES256);
        header.kid = kid.map(str::to_owned);
        encode(&header, &claims(3600), &enc).unwrap()
    }

    #[test]
    fn known_kid_verified_against_matching_key() {
        let default_keys = gen_ec_keys();
        let rotated = gen_ec_keys();
        let mut by_kid = HashMap::new();
        by_kid.insert(
            "2026-q3".to_string(),
            std::str::from_utf8(&rotated.pub_pem).unwrap().to_string(),
        );
        let jwtkeys = JwtKeys::new(
            std::str::from_utf8(&default_keys.pub_pem)
                .unwrap()
                .to_string(),
            by_kid,
        );

        let jwt = sign_with_kid(&rotated.priv_pkcs8_pem, Some("2026-q3"));
        let v = verify_jwt(&jwt, &jwtkeys, &NoRevocations, now() as u64, 0).unwrap();
        assert_eq!(v.host_id, "host-a");
    }

    #[test]
    fn unknown_kid_rejected() {
        let default_keys = gen_ec_keys();
        let jwtkeys = JwtKeys::single(
            std::str::from_utf8(&default_keys.pub_pem)
                .unwrap()
                .to_string(),
        );
        let jwt = sign_with_kid(&default_keys.priv_pkcs8_pem, Some("nope"));
        let err = verify_jwt(&jwt, &jwtkeys, &NoRevocations, now() as u64, 0)
            .err()
            .unwrap();
        assert!(err.to_string().contains("unknown JWT kid"), "got: {err}");
    }

    #[test]
    fn no_kid_falls_back_to_default_key() {
        let default_keys = gen_ec_keys();
        let jwtkeys = JwtKeys::single(
            std::str::from_utf8(&default_keys.pub_pem)
                .unwrap()
                .to_string(),
        );
        let jwt = sign_with_kid(&default_keys.priv_pkcs8_pem, None);
        let v = verify_jwt(&jwt, &jwtkeys, &NoRevocations, now() as u64, 0).unwrap();
        assert_eq!(v.host_id, "host-a");
    }

    #[test]
    fn flux_default_kid_uses_default_key() {
        let default_keys = gen_ec_keys();
        let jwtkeys = JwtKeys::single(
            std::str::from_utf8(&default_keys.pub_pem)
                .unwrap()
                .to_string(),
        );
        let jwt = sign_with_kid(&default_keys.priv_pkcs8_pem, Some(DEFAULT_KID));
        let v = verify_jwt(&jwt, &jwtkeys, &NoRevocations, now() as u64, 0).unwrap();
        assert_eq!(v.host_id, "host-a");
    }

    // ── #2: tenant (team_id) claim ────────────────────────────────────────

    #[test]
    fn team_id_surfaced_from_verified_token() {
        let keys = gen_ec_keys();
        let enc = EncodingKey::from_ec_pem(&keys.priv_pkcs8_pem).unwrap();
        let jwt = encode(&Header::new(Algorithm::ES256), &claims(3600), &enc).unwrap();
        let v = verify_default(&jwt, &keys.pub_pem).unwrap();
        assert_eq!(v.team_id.as_deref(), Some("team-1"));
    }

    #[test]
    fn team_id_required_rejects_missing_and_blank() {
        assert!(!team_id_satisfied(None, true));
        assert!(!team_id_satisfied(Some(""), true));
        assert!(!team_id_satisfied(Some("   "), true));
        assert!(team_id_satisfied(Some("team-1"), true));
    }

    #[test]
    fn team_id_not_required_tolerates_missing() {
        assert!(team_id_satisfied(None, false));
        assert!(team_id_satisfied(Some(""), false));
        assert!(team_id_satisfied(Some("team-1"), false));
    }

    // ── #2: host binding ──────────────────────────────────────────────────

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn host_binding_transport_peer_match_accepted() {
        let d = decide_host_binding("10.42.0.7", "10.42.0.7", Some(ip("10.42.0.7")), true);
        assert_eq!(d, Ok(HostBinding::Transport));
    }

    #[test]
    fn host_binding_transport_peer_mismatch_rejected_when_required() {
        // Stolen token (sub=host A) replayed from a different host IP.
        let err =
            decide_host_binding("10.42.0.7", "10.42.0.7", Some(ip("10.42.0.9")), true).unwrap_err();
        assert!(err.contains("does not match token sub"), "got: {err}");
    }

    #[test]
    fn host_binding_transport_peer_mismatch_tolerated_when_not_required() {
        let d =
            decide_host_binding("10.42.0.7", "10.42.0.7", Some(ip("10.42.0.9")), false).unwrap();
        assert!(matches!(d, HostBinding::Unenforced { .. }));
    }

    #[test]
    fn host_binding_falls_back_to_hello_when_peer_unavailable() {
        // No transport peer → verify sub against Hello-advertised mgmt IP.
        let d = decide_host_binding("10.42.0.7", "10.42.0.7", None, true).unwrap();
        assert!(matches!(d, HostBinding::HelloFallback { .. }));
    }

    #[test]
    fn host_binding_hello_mismatch_rejected_when_required() {
        let err = decide_host_binding("10.42.0.7", "10.42.0.9", None, true).unwrap_err();
        assert!(err.contains("does not match token sub"), "got: {err}");
    }

    #[test]
    fn host_binding_hello_mismatch_tolerated_when_not_required() {
        let d = decide_host_binding("10.42.0.7", "10.42.0.9", None, false).unwrap();
        assert!(matches!(d, HostBinding::Unenforced { .. }));
    }

    #[test]
    fn host_binding_non_ip_sub_degrades_to_hello_check() {
        // A UUID-style host id can't be compared to a peer IP; degrade to the
        // Hello check rather than break the mesh.
        let d = decide_host_binding(
            "host-uuid-abc",
            "host-uuid-abc",
            Some(ip("10.42.0.7")),
            true,
        )
        .unwrap();
        assert!(matches!(d, HostBinding::HelloFallback { .. }));
    }
}
