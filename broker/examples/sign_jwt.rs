//! Generate ES256 keypair and sign a test JWT.
//!
//! Usage:
//!   cargo run -p broker --example sign_jwt -- <host_id> <priv_pem_out> <pub_pem_out> [caps]
//!
//! `caps` is an optional comma-separated capability list; defaults to "coold".
//! Example: `coold,builder` for a host that should accept build dispatches.
//!
//! Prints the signed JWT to stdout.

use std::fs;
use std::process::Command;

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

#[derive(Serialize)]
struct Claims {
    sub: String,
    aud: String,
    caps: Vec<String>,
    exp: usize,
    iat: usize,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: sign_jwt <host_id> <priv_pem_out> <pub_pem_out> [caps]");
        std::process::exit(2);
    }
    let host_id = &args[1];
    let priv_path = &args[2];
    let pub_path = &args[3];
    let caps: Vec<String> = args
        .get(4)
        .map(|s| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect())
        .unwrap_or_else(|| vec!["coold".to_string()]);

    let status = Command::new("openssl")
        .args(["ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", priv_path])
        .status()?;
    anyhow::ensure!(status.success(), "openssl ecparam failed");

    let pkcs8_path = format!("{priv_path}.pkcs8");
    let status = Command::new("openssl")
        .args(["pkcs8", "-topk8", "-nocrypt", "-in", priv_path, "-out", &pkcs8_path])
        .status()?;
    anyhow::ensure!(status.success(), "openssl pkcs8 failed");

    let status = Command::new("openssl")
        .args(["ec", "-in", priv_path, "-pubout", "-out", pub_path])
        .status()?;
    anyhow::ensure!(status.success(), "openssl ec pubout failed");

    let priv_pem = fs::read(&pkcs8_path)?;
    let key = EncodingKey::from_ec_pem(&priv_pem)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as usize;
    let claims = Claims {
        sub: host_id.clone(),
        aud: "coold".into(),
        caps,
        exp: now + 3600,
        iat: now,
    };

    let jwt = encode(&Header::new(Algorithm::ES256), &claims, &key)?;
    println!("{jwt}");
    Ok(())
}
