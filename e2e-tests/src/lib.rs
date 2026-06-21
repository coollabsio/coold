//! Live-server test harness for the coold/flux stack.
//!
//! Tests are Rust integration tests under `tests/`, marked `#[ignore]` so
//! default `cargo test` skips them. Every suite provisions its own
//! ephemeral Hetzner cluster via [`hetzner::EphemeralCluster`], runs
//! `coolify init bootstrap` from the local `coolify` binary, then exercises
//! the black-box HTTP/UDS/systemd contract over SSH. No flux/coold
//! code is linked.
//!
//! Run with:
//!
//! ```text
//! HETZNER_TOKEN=... HETZNER_PROJECT=... \
//! SSH_KEY=~/.ssh/<key> \
//! COOLIFY_CLI_BIN=$(which coolify) \
//! cargo test -p e2e-tests -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` is recommended because the suites provision live VMs.
//! VMs are deleted via [`hetzner::EphemeralCluster`]'s RAII `Drop`, so
//! panics during assertions still clean up paid resources.

use std::thread;
use std::time::{Duration, Instant};

pub mod hetzner;
pub mod install;

use std::cell::RefCell;

thread_local! {
    static TAG: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Set a short prefix included by [`log_step`] / [`log_ok`] / hetzner
/// progress lines. Each test calls this at entry so interleaved parallel
/// output can be disambiguated.
pub fn set_tag(t: impl Into<String>) {
    TAG.with(|c| *c.borrow_mut() = t.into());
}

pub fn tag() -> String {
    TAG.with(|c| c.borrow().clone())
}

/// Prefix `msg` with the thread-local tag (if set) and emit to stderr.
pub fn log_line(msg: &str) {
    let t = tag();
    if t.is_empty() {
        eprintln!("{msg}");
    } else {
        eprintln!("[{t}] {msg}");
    }
}

/// Populate `std::env` from `<crate>/.env` if the file exists. Values in the
/// file never override existing env vars — a shell-exported var always wins.
/// Idempotent across calls; safe to invoke from every `from_env()`.
pub fn load_dotenv() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }
}

pub fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_secs(1));
    }
    false
}
