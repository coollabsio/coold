pub mod coold;
pub mod corrosion;
pub mod tls;

/// S1-adjacent (supply-chain): render a shell snippet that verifies a
/// downloaded release artifact's SHA-256 before it is installed.
///
/// Binary installs fetch release tarballs over HTTPS but historically did no
/// integrity check, so a compromised release host or MITM (TLS-terminating
/// proxy) could swap the artifact. This snippet closes that gap:
///
///   * When `sha256` is `Some(digest)` the operator pinned an expected digest;
///     the artifact is compared against it and the install ABORTS on mismatch.
///   * When `sha256` is `None` the snippet attempts to fetch the release's
///     published `<url>.sha256` sidecar and verifies against it if present;
///     it aborts on mismatch but tolerates an absent sidecar (so moving
///     targets like `nightly` still install). Operators can enforce
///     verification unconditionally by pinning a digest.
///
/// `tarball` is a shell expression for the downloaded file (e.g.
/// `"$DLDIR/coold.tar.gz"`), `url` is the shell variable holding the artifact
/// URL (e.g. `"$URL"`), and `artifact` names it for error messages.
pub fn checksum_verify_snippet(
    tarball: &str,
    url: &str,
    artifact: &str,
    sha256: Option<&str>,
) -> String {
    match sha256 {
        Some(digest) => format!(
            "printf '%s  %s\\n' '{digest}' \"{tarball}\" | sha256sum -c - >/dev/null 2>&1 || {{ echo \"{artifact}: sha256 mismatch (expected pinned {digest})\" >&2; exit 1; }}"
        ),
        None => format!(
            "if curl -fsSL --retry 3 --max-time 60 -o \"{tarball}.sha256\" \"{url}.sha256\" 2>/dev/null; then \
printf '%s  %s\\n' \"$(awk '{{print $1}}' \"{tarball}.sha256\")\" \"{tarball}\" | sha256sum -c - >/dev/null 2>&1 || {{ echo \"{artifact}: sha256 mismatch vs published checksum\" >&2; exit 1; }}; \
else echo \"{artifact}: no published sha256 to verify against; proceeding (pin one to enforce)\" >&2; fi"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_checksum_snippet_aborts_on_mismatch() {
        let snip = checksum_verify_snippet("$DLDIR/coold.tar.gz", "$URL", "coold", Some("abc123"));
        assert!(snip.contains("abc123"));
        assert!(snip.contains("sha256sum -c -"));
        assert!(snip.contains("exit 1"));
        assert!(!snip.contains(".sha256\""));
    }

    #[test]
    fn unpinned_checksum_snippet_uses_published_sidecar_but_tolerates_absence() {
        let snip = checksum_verify_snippet("$DLDIR/coold.tar.gz", "$URL", "coold", None);
        assert!(snip.contains("$URL.sha256"));
        assert!(snip.contains("sha256sum -c -"));
        assert!(snip.contains("exit 1"));
        assert!(snip.contains("proceeding"));
    }

    // Behavior tests: execute the generated snippet under `sh` with stubbed
    // `curl` and `sha256sum` on PATH so we verify the branching (proceed vs
    // abort), not just the emitted text. `sha256sum` exit is driven by
    // `STUB_SHA_OK`; `curl` writes/omits the sidecar per `STUB_SIDECAR`.
    #[cfg(unix)]
    mod behavior {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        const CURL_STUB: &str = "#!/bin/sh\nout=\"\"\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) out=\"$2\"; shift 2 ;; *) shift ;; esac; done\nif [ \"$STUB_SIDECAR\" = present ]; then printf '%s  coold.tar.gz\\n' \"deadbeef\" > \"$out\"; exit 0; fi\nexit 1\n";
        const SHA_STUB: &str = "#!/bin/sh\ncat >/dev/null 2>&1\n[ \"$STUB_SHA_OK\" = 1 ]\n";

        /// Run `snippet` with the stubs; return true when it exits 0 (install
        /// would proceed), false when it aborts (non-zero exit).
        fn proceeds(snippet: &str, sidecar_present: bool, sha_ok: bool) -> bool {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            for (name, body) in [("curl", CURL_STUB), ("sha256sum", SHA_STUB)] {
                let p = bin.join(name);
                std::fs::write(&p, body).unwrap();
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let tarball = dir.path().join("coold.tar.gz");
            std::fs::write(&tarball, b"tarball-bytes").unwrap();
            let snippet = snippet
                .replace("TARBALL", tarball.to_str().unwrap())
                .replace("URL", "https://example.test/coold.tar.gz");
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let status = Command::new("sh")
                .arg("-c")
                .arg(&snippet)
                .env("PATH", path)
                .env(
                    "STUB_SIDECAR",
                    if sidecar_present { "present" } else { "absent" },
                )
                .env("STUB_SHA_OK", if sha_ok { "1" } else { "0" })
                .status()
                .unwrap();
            status.success()
        }

        #[test]
        fn sidecar_present_and_matches_proceeds() {
            let snip = checksum_verify_snippet("TARBALL", "URL", "coold", None);
            assert!(proceeds(&snip, true, true));
        }

        #[test]
        fn sidecar_present_and_mismatch_aborts() {
            let snip = checksum_verify_snippet("TARBALL", "URL", "coold", None);
            assert!(!proceeds(&snip, true, false));
        }

        #[test]
        fn pinned_digest_mismatch_aborts() {
            let snip = checksum_verify_snippet("TARBALL", "URL", "coold", Some("deadbeef"));
            assert!(!proceeds(&snip, false, false));
        }

        #[test]
        fn pinned_digest_match_proceeds() {
            let snip = checksum_verify_snippet("TARBALL", "URL", "coold", Some("deadbeef"));
            assert!(proceeds(&snip, false, true));
        }

        #[test]
        fn nothing_available_warns_and_proceeds() {
            // Unpinned + sidecar absent: verification is skipped with a warning,
            // install still proceeds.
            let snip = checksum_verify_snippet("TARBALL", "URL", "coold", None);
            assert!(proceeds(&snip, false, false));
        }
    }
}
