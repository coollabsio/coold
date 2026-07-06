//! Host JWT rotation applied over the gRPC stream (verb `host.jwt.set`).
//!
//! Seamless rotation flow: Laravel rotates the host token while the current one
//! is still valid (the stream is up), pushes the new token here, and coold
//! validates it and atomically writes it to `host_jwt_path`. coold does NOT
//! reconnect or restart on receipt — the new token is picked up on the NEXT
//! reconnect, which the current token's `exp` drives (flux drops the stream at
//! exp and coold re-reads the file on every reconnect). SSH delivery remains the
//! Laravel-side fallback for when the stream is down.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Validate a pushed host JWT and, if it passes, atomically install it at
/// `host_jwt_path`.
///
/// Validation (never brick the host — validate before writing):
/// - Reject an empty jwt or one that is not structurally a JWT (three
///   non-empty base64url segments `xxx.yyy.zzz`).
/// - Decode the payload segment; when coold can determine its own host id from
///   the currently-installed token's `sub`, require the new token's `sub` to
///   match (prevents installing a token minted for a different host). When
///   coold cannot determine its own id, structural validation is sufficient.
/// - The signature is NOT verified: coold has no public key and flux already
///   authenticated the dispatch.
pub(crate) async fn apply_host_jwt(new_jwt: &str, host_jwt_path: &Path) -> Result<()> {
    let new_jwt = new_jwt.trim();
    let new_sub = validate_and_extract_sub(new_jwt)?;

    if let Some(new_sub) = new_sub.as_deref() {
        if let Some(current_sub) = current_host_sub(host_jwt_path).await {
            if current_sub != new_sub {
                return Err(anyhow!(
                    "refusing host JWT: token sub {new_sub:?} does not match current host {current_sub:?}"
                ));
            }
        }
    }

    write_host_jwt_atomic(host_jwt_path, new_jwt).await
}

/// Structurally validate a JWT string and return its `sub` claim if present.
///
/// Requires exactly three non-empty, base64url-decodable segments. Returns the
/// payload `sub` when the payload decodes to JSON carrying a string `sub`;
/// otherwise `Ok(None)` (structural validity without a determinable subject).
pub(crate) fn validate_and_extract_sub(jwt: &str) -> Result<Option<String>> {
    if jwt.is_empty() {
        return Err(anyhow!("host JWT is empty"));
    }

    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(anyhow!(
            "host JWT is not a structurally valid token (want three non-empty base64url segments)"
        ));
    }

    let mut decoded = Vec::with_capacity(3);
    for part in &parts {
        let bytes = base64url_decode(part)
            .ok_or_else(|| anyhow!("host JWT segment is not valid base64url"))?;
        decoded.push(bytes);
    }

    let sub = serde_json::from_slice::<serde_json::Value>(&decoded[1])
        .ok()
        .and_then(|payload| {
            payload
                .get("sub")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        });

    Ok(sub)
}

/// Best-effort read of the currently-installed host JWT's `sub`. Returns `None`
/// when the file is missing/unreadable or carries no determinable subject.
async fn current_host_sub(host_jwt_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(host_jwt_path).await.ok()?;
    validate_and_extract_sub(raw.trim()).ok().flatten()
}

/// Atomically install `jwt` at `path`: write to a sibling temp file with mode
/// 0600, then rename over the target. A rename is atomic on the same
/// filesystem, so a reader never observes a half-written token.
async fn write_host_jwt_atomic(path: &Path, jwt: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("host-jwt");
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));

    let write_result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .await
            .with_context(|| format!("open temp host JWT {}", tmp.display()))?;
        file.write_all(jwt.as_bytes())
            .await
            .context("write temp host JWT")?;
        file.flush().await.context("flush temp host JWT")?;
        // Guarantee exactly 0600 regardless of the process umask.
        fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .await
            .context("chmod temp host JWT")?;
        fs::rename(&tmp, path)
            .await
            .with_context(|| format!("install host JWT at {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if write_result.is_err() {
        // Best-effort cleanup so a failed apply never leaves a stray temp token.
        let _ = fs::remove_file(&tmp).await;
    }

    write_result
}

/// Decode a base64url (no-padding) string. Returns `None` on any invalid byte.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in input.as_bytes() {
        buffer = (buffer << 6) | sextet(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for &byte in bytes {
            buffer = (buffer << 8) | byte as u32;
            bits += 8;
            while bits >= 6 {
                bits -= 6;
                out.push(ALPHABET[((buffer >> bits) & 0x3f) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHABET[((buffer << (6 - bits)) & 0x3f) as usize] as char);
        }
        out
    }

    /// Build a structurally valid JWT with the given `sub` claim.
    fn jwt_with_sub(sub: &str) -> String {
        let header = b64url(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64url(format!(r#"{{"sub":"{sub}"}}"#).as_bytes());
        let signature = b64url(b"signature-bytes");
        format!("{header}.{payload}.{signature}")
    }

    #[tokio::test]
    async fn writes_valid_jwt_atomically_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-jwt");

        apply_host_jwt("a.b.c", &path).await.unwrap();

        assert_eq!(fs::read_to_string(&path).await.unwrap(), "a.b.c");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got mode {mode:#o}");
        // No temp file left behind.
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[tokio::test]
    async fn rejects_empty_jwt_without_touching_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-jwt");
        fs::write(&path, "existing.token.value").await.unwrap();

        let err = apply_host_jwt("   ", &path).await.unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "existing.token.value"
        );
    }

    #[tokio::test]
    async fn rejects_malformed_jwt_without_touching_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-jwt");
        fs::write(&path, "existing.token.value").await.unwrap();

        // Only two segments — not structurally a JWT.
        let err = apply_host_jwt("only.two", &path).await.unwrap_err();
        assert!(format!("{err:#}").contains("structurally valid"));
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "existing.token.value"
        );
    }

    #[tokio::test]
    async fn rejects_non_base64url_segment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-jwt");

        // '*' is outside the base64url alphabet.
        let err = apply_host_jwt("aa.b*c.dd", &path).await.unwrap_err();
        assert!(format!("{err:#}").contains("base64url"));
        assert!(fs::metadata(&path).await.is_err(), "must not create file");
    }

    #[tokio::test]
    async fn rejects_token_whose_sub_differs_from_current_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-jwt");
        let current = jwt_with_sub("host-abc");
        fs::write(&path, &current).await.unwrap();

        let mismatched = jwt_with_sub("host-xyz");
        let err = apply_host_jwt(&mismatched, &path).await.unwrap_err();
        assert!(format!("{err:#}").contains("does not match current host"));
        // Existing token preserved.
        assert_eq!(fs::read_to_string(&path).await.unwrap(), current);
    }

    #[tokio::test]
    async fn accepts_token_whose_sub_matches_current_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host-jwt");
        fs::write(&path, jwt_with_sub("host-abc")).await.unwrap();

        let rotated = jwt_with_sub("host-abc");
        apply_host_jwt(&rotated, &path).await.unwrap();
        assert_eq!(fs::read_to_string(&path).await.unwrap(), rotated);
    }

    #[test]
    fn extracts_sub_from_payload() {
        let jwt = jwt_with_sub("host-42");
        assert_eq!(
            validate_and_extract_sub(&jwt).unwrap(),
            Some("host-42".to_string())
        );
    }

    #[test]
    fn structurally_valid_without_json_payload_has_no_sub() {
        assert_eq!(validate_and_extract_sub("a.b.c").unwrap(), None);
    }
}
