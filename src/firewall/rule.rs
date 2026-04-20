//! Pure rule types: identity hash, iptables match-arg rendering, parsing.
//!
//! Zero side effects. Keeps the side-effecting `store` module focused on
//! process execution and file IO. Format is kept strictly compatible with
//! the Go CLI's `internal/firewall/rule.go` so mixed writers produce
//! compatible state on disk and in the kernel.

use std::net::IpAddr;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// An ACCEPT rule in the COOLIFY-ALLOW chain.
///
/// `proto` is `Some("tcp")` / `Some("udp")` or `None` (match any protocol).
/// `port` is `Some(n)` (dst port) or `None` (match any port; only valid when
/// `proto` is also `None`, since `--dport` requires `-p`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowRule {
    pub src: IpAddr,
    pub dst: IpAddr,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub proto: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub port: Option<u16>,
    /// Stable 12-hex identity. Clients may omit on create; the server
    /// computes it from (src, dst, proto, port) so identical tuples hash
    /// to identical IDs regardless of creator.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
}

/// Compute the 12-hex stable identity for a (src, dst, proto, port) tuple.
///
/// Matches the Go CLI byte-for-byte: `sha256("<src>|<dst>|<proto>|<port>")`
/// truncated to the first 12 hex chars, proto lowercased, `None` proto
/// rendered as empty, `None` port rendered as `0`.
#[allow(non_snake_case)]
pub fn ComputeID_(src: &IpAddr, dst: &IpAddr, proto: Option<&str>, port: Option<u16>) -> String {
    let proto_s = proto.unwrap_or("").to_ascii_lowercase();
    let port_n = port.unwrap_or(0);
    let material = format!("{src}|{dst}|{proto_s}|{port_n}");
    let digest = Sha256::digest(material.as_bytes());
    hex::encode(digest)[..12].to_string()
}

impl AllowRule {
    /// Validate user-supplied fields and fill in `id` if missing.
    ///
    /// Rejects: `--dport` without `-p` (kernel requires proto for port
    /// match); proto values outside {tcp, udp}; private-use port 0 with
    /// proto set (would iptables-accept but has no semantic meaning for
    /// user-facing allow rules).
    pub fn normalize(mut self) -> Result<Self> {
        if let Some(p) = &self.proto {
            let lower = p.to_ascii_lowercase();
            if lower != "tcp" && lower != "udp" {
                return Err(anyhow!("proto must be tcp or udp, got {p:?}"));
            }
            self.proto = Some(lower);
        }
        if self.port.is_some() && self.proto.is_none() {
            return Err(anyhow!("port requires proto (tcp or udp)"));
        }
        if matches!(self.port, Some(0)) {
            return Err(anyhow!("port 0 is not a valid match"));
        }
        let computed = ComputeID_(&self.src, &self.dst, self.proto.as_deref(), self.port);
        match &self.id {
            Some(supplied) if supplied != &computed => {
                return Err(anyhow!(
                    "supplied id {supplied:?} does not match tuple hash {computed:?}"
                ));
            }
            _ => self.id = Some(computed),
        }
        Ok(self)
    }

    /// The common iptables match portion. Kept identical to the Go CLI's
    /// `matchArgs()` so snapshots are byte-stable across writers.
    pub fn match_args(&self, chain: &str) -> Vec<String> {
        let _ = chain; // chain is added by caller for -A/-D/-C
        let mut args = vec![
            "-s".into(),
            self.src.to_string(),
            "-d".into(),
            self.dst.to_string(),
        ];
        if let Some(proto) = &self.proto {
            args.push("-p".into());
            args.push(proto.clone());
            if let Some(port) = self.port {
                args.push("--dport".into());
                args.push(port.to_string());
            }
        }
        if let Some(id) = &self.id {
            args.push("-m".into());
            args.push("comment".into());
            args.push("--comment".into());
            args.push(format!("cid:{id}"));
        }
        args.push("-j".into());
        args.push("ACCEPT".into());
        args
    }

    /// Full arg vector for `iptables <op> <chain> <match>`. `op` is one of
    /// `-A`, `-D`, `-C`.
    pub fn chain_args(&self, op: &str, chain: &str) -> Vec<String> {
        let mut args = vec![op.into(), chain.into()];
        args.extend(self.match_args(chain));
        args
    }
}

/// Parse one `-A <chain> ...` line from `iptables -S <chain>` into an
/// AllowRule. Returns `None` for non-append lines or unparseable entries.
///
/// Mirrors `ParseChainLine` in the Go CLI, minus the go regex engine —
/// a small hand-rolled tokenizer suffices.
pub fn parse_chain_line(line: &str, chain: &str) -> Option<AllowRule> {
    let line = line.trim();
    let prefix = format!("-A {chain} ");
    let rest = line.strip_prefix(&prefix)?;

    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut src: Option<IpAddr> = None;
    let mut dst: Option<IpAddr> = None;
    let mut proto: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut comment: Option<String> = None;

    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "-s" if i + 1 < tokens.len() => {
                src = parse_ip_maybe_cidr(tokens[i + 1]);
                i += 2;
            }
            "-d" if i + 1 < tokens.len() => {
                dst = parse_ip_maybe_cidr(tokens[i + 1]);
                i += 2;
            }
            "-p" if i + 1 < tokens.len() => {
                proto = Some(tokens[i + 1].to_ascii_lowercase());
                i += 2;
            }
            "--dport" if i + 1 < tokens.len() => {
                port = tokens[i + 1].parse().ok();
                i += 2;
            }
            "--comment" if i + 1 < tokens.len() => {
                // Comment may be quoted or bare. iptables -S quotes
                // multi-word comments; ours are single-token "cid:<hex>".
                let raw = tokens[i + 1].trim_matches('"');
                comment = Some(raw.to_string());
                i += 2;
            }
            _ => i += 1,
        }
    }

    let src = src?;
    let dst = dst?;
    let id = comment.and_then(|c| c.strip_prefix("cid:").map(|s| s.to_string()));

    Some(AllowRule {
        src,
        dst,
        proto,
        port,
        id,
    })
}

fn parse_ip_maybe_cidr(s: &str) -> Option<IpAddr> {
    let bare = s
        .strip_suffix("/32")
        .or_else(|| s.strip_suffix("/128"))
        .unwrap_or(s);
    bare.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn compute_id_matches_go_cli_shape() {
        // Shape test: same inputs → deterministic 12-hex. We cannot assert
        // against the Go CLI without embedding the expected value, but the
        // formula (proto lowercased, empty for None, port 0 for None) is
        // the contract the Go CLI's ComputeID documents.
        let id = ComputeID_(
            &ipv4("10.210.5.2"),
            &ipv4("10.210.6.3"),
            Some("tcp"),
            Some(80),
        );
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

        let id2 = ComputeID_(
            &ipv4("10.210.5.2"),
            &ipv4("10.210.6.3"),
            Some("TCP"),
            Some(80),
        );
        assert_eq!(id, id2, "proto must be case-insensitive");
    }

    #[test]
    fn normalize_fills_id() {
        let r = AllowRule {
            src: ipv4("10.210.5.2"),
            dst: ipv4("10.210.6.3"),
            proto: Some("tcp".into()),
            port: Some(80),
            id: None,
        }
        .normalize()
        .unwrap();
        assert!(r.id.is_some());
    }

    #[test]
    fn normalize_rejects_port_without_proto() {
        let r = AllowRule {
            src: ipv4("10.0.0.1"),
            dst: ipv4("10.0.0.2"),
            proto: None,
            port: Some(80),
            id: None,
        }
        .normalize();
        assert!(r.is_err());
    }

    #[test]
    fn normalize_rejects_bad_proto() {
        let r = AllowRule {
            src: ipv4("10.0.0.1"),
            dst: ipv4("10.0.0.2"),
            proto: Some("icmp".into()),
            port: None,
            id: None,
        }
        .normalize();
        assert!(r.is_err());
    }

    #[test]
    fn normalize_rejects_mismatched_supplied_id() {
        let r = AllowRule {
            src: ipv4("10.0.0.1"),
            dst: ipv4("10.0.0.2"),
            proto: Some("tcp".into()),
            port: Some(80),
            id: Some("deadbeefcafe".into()),
        }
        .normalize();
        assert!(r.is_err());
    }

    #[test]
    fn chain_args_render_with_cid() {
        let r = AllowRule {
            src: ipv4("10.210.5.2"),
            dst: ipv4("10.210.6.3"),
            proto: Some("tcp".into()),
            port: Some(80),
            id: None,
        }
        .normalize()
        .unwrap();
        let args = r.chain_args("-A", "COOLIFY-ALLOW");
        assert_eq!(args[0], "-A");
        assert_eq!(args[1], "COOLIFY-ALLOW");
        assert!(args.contains(&"-s".to_string()));
        assert!(args.contains(&"10.210.5.2".to_string()));
        assert!(args.contains(&"--dport".to_string()));
        assert!(args.contains(&"80".to_string()));
        let comment_idx = args.iter().position(|a| a == "--comment").unwrap();
        assert!(args[comment_idx + 1].starts_with("cid:"));
    }

    #[test]
    fn parse_chain_line_round_trip() {
        let original = AllowRule {
            src: ipv4("10.210.5.2"),
            dst: ipv4("10.210.6.3"),
            proto: Some("tcp".into()),
            port: Some(443),
            id: None,
        }
        .normalize()
        .unwrap();

        // Simulate iptables -S output shape.
        let line = format!(
            r#"-A COOLIFY-ALLOW -s {src}/32 -d {dst}/32 -p tcp -m tcp --dport {port} -m comment --comment "cid:{id}" -j ACCEPT"#,
            src = original.src,
            dst = original.dst,
            port = original.port.unwrap(),
            id = original.id.as_ref().unwrap(),
        );
        let parsed = parse_chain_line(&line, "COOLIFY-ALLOW").unwrap();
        assert_eq!(parsed.src, original.src);
        assert_eq!(parsed.dst, original.dst);
        assert_eq!(parsed.proto.as_deref(), Some("tcp"));
        assert_eq!(parsed.port, Some(443));
        assert_eq!(parsed.id, original.id);
    }

    #[test]
    fn parse_chain_line_rejects_non_append() {
        assert!(parse_chain_line("-N COOLIFY-ALLOW", "COOLIFY-ALLOW").is_none());
        assert!(parse_chain_line("-A OTHER -s 1.2.3.4 -d 5.6.7.8 -j ACCEPT", "COOLIFY-ALLOW").is_none());
    }
}
