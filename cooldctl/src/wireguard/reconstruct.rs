use anyhow::Result;
use ipnet::Ipv4Net;
use sha2::{Digest, Sha256};

use super::state::{MeshState, NamespaceServerState, Peer, ServerState};
use crate::ssh::{for_each_server, Runner};

pub async fn reconstruct<R: Runner>(
    runner: &R,
    hosts: &[String],
    user: &str,
    port: u16,
    iface: &str,
    namespaces: &[String],
    concurrency: usize,
) -> Result<MeshState> {
    let results = for_each_server(hosts, concurrency, |host| async move {
        probe(runner, &host, user, port, iface, namespaces).await
    })
    .await;
    let mut state = MeshState::default();
    let mut errs = vec![];
    for r in results {
        if let Some(s) = r.result {
            state.servers.insert(r.host, s);
        } else if let Some(e) = r.error {
            errs.push(format!("{}: {}", r.host, e));
        }
    }
    if !errs.is_empty() {
        eprintln!("Warning: probe failed on some hosts: {}", errs.join("; "));
    }
    Ok(state)
}

async fn probe<R: Runner>(
    runner: &R,
    host: &str,
    user: &str,
    port: u16,
    iface: &str,
    namespaces: &[String],
) -> Result<ServerState> {
    let script = format!(
        r#"set +e
printf 'WG_INSTALLED='; command -v wg >/dev/null 2>&1 && echo 1 || echo 0
printf 'KEYS_EXIST='; test -s /etc/wireguard/publickey && test -s /etc/wireguard/privatekey && echo 1 || echo 0
printf 'PUBLIC_KEY='; cat /etc/wireguard/publickey 2>/dev/null || true; echo
printf 'CONFIG_B64='; base64 -w0 /etc/wireguard/{iface}.conf 2>/dev/null || true; echo
printf 'WG_DUMP_B64='; wg show {iface} dump 2>/dev/null | base64 -w0 || true; echo
printf 'PODMAN_INSTALLED='; command -v podman >/dev/null 2>&1 && echo 1 || echo 0
printf 'PODMAN_SOCKET_ACTIVE='; systemctl is-active --quiet podman.socket && echo 1 || echo 0
printf 'IP_FORWARD='; cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo 0
printf 'FW_ACTIVE='; systemctl is-active --quiet coolify-mesh-fw.service && echo 1 || echo 0
printf 'DEFAULT_DENY='; iptables -S COOLIFY-INTRA 2>/dev/null | grep -q -- '-A COOLIFY-INTRA -j DROP' && echo 1 || echo 0
printf 'FW_HASH='; sha256sum /etc/systemd/system/coolify-mesh-fw.service 2>/dev/null | awk '{{print $1}}'; echo
printf 'NFT_AVAILABLE='; command -v nft >/dev/null 2>&1 && echo 1 || echo 0
printf 'BRIDGE_TABLE='; nft list table bridge coolify_bridge >/dev/null 2>&1 && echo 1 || echo 0
printf 'CORROSION_INSTALLED='; test -x /usr/local/bin/corrosion && echo 1 || echo 0
printf 'CORROSION_ACTIVE='; systemctl is-active --quiet corrosion && echo 1 || echo 0
printf 'CORROSION_CONFIG_HASH='; sha256sum /etc/corrosion/config.toml 2>/dev/null | awk '{{print $1}}'; echo
printf 'CORROSION_SCHEMA_EXISTS='; test -s /etc/corrosion/schemas/coolify.sql && echo 1 || echo 0
printf 'CORROSION_SCHEMA_HASH='; sha256sum /etc/corrosion/schemas/coolify.sql 2>/dev/null | awk '{{print $1}}'; echo
printf 'COOLIFY_COOLD_INSTALLED='; test -x /usr/local/bin/coold && echo 1 || echo 0
printf 'COOLIFY_COOLD_ACTIVE='; systemctl is-active --quiet coold && echo 1 || echo 0
printf 'CORROSION_VERSION='; cat /usr/local/bin/corrosion.version 2>/dev/null || true; echo
printf 'COOLIFY_COOLD_VERSION='; cat /usr/local/bin/coold.version 2>/dev/null || true; echo
printf 'COOLIFY_COOLD_UNIT_HASH='; sha256sum /etc/systemd/system/coold.service 2>/dev/null | awk '{{print $1}}'; echo
for ns in {nslist}; do net="coolify-${{ns}}-mesh"; printf 'NS=%s|' "$ns"; podman network inspect "$net" --format '{{{{.Name}}}}|{{{{range .Subnets}}}}{{{{.Subnet}}}}{{{{end}}}}|{{{{.DNSEnabled}}}}|{{{{index .Labels "io.coolify.namespace"}}}}' 2>/dev/null || echo 'missing|||'; done
"#,
        nslist = namespaces.join(" ")
    );
    let out = runner.run(host, user, port, &script).await?;
    Ok(parse_probe(host, iface, namespaces, &out.stdout))
}

fn parse_probe(host: &str, iface: &str, namespaces: &[String], text: &str) -> ServerState {
    let mut s = ServerState {
        host: host.into(),
        interface: iface.into(),
        ..Default::default()
    };
    let mut config = String::new();
    let mut wg_dump = String::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "WG_INSTALLED" => s.installed = v.trim() == "1",
            "KEYS_EXIST" => s.keys_exist = v.trim() == "1",
            "PUBLIC_KEY" => s.public_key = v.trim().into(),
            "CONFIG_B64" => config = decode_b64(v),
            "WG_DUMP_B64" => wg_dump = decode_b64(v),
            "PODMAN_INSTALLED" => s.podman_installed = v.trim() == "1",
            "PODMAN_SOCKET_ACTIVE" => s.podman_socket_active = v.trim() == "1",
            "IP_FORWARD" => s.ip_forward_enabled = v.trim() == "1",
            "FW_ACTIVE" => s.firewall_active = v.trim() == "1",
            "DEFAULT_DENY" => s.default_deny_active = v.trim() == "1",
            "FW_HASH" => s.firewall_unit_sha256 = v.trim().into(),
            "NFT_AVAILABLE" => s.nft_available = v.trim() == "1",
            "BRIDGE_TABLE" => s.bridge_table_exists = v.trim() == "1",
            "CORROSION_INSTALLED" => s.corrosion_installed = v.trim() == "1",
            "CORROSION_ACTIVE" => s.corrosion_active = v.trim() == "1",
            "CORROSION_CONFIG_HASH" => s.corrosion_config_hash = v.trim().into(),
            "CORROSION_SCHEMA_EXISTS" => s.corrosion_schema_exists = v.trim() == "1",
            "CORROSION_SCHEMA_HASH" => s.corrosion_schema_sha256 = v.trim().into(),
            "COOLIFY_COOLD_INSTALLED" => s.coold_installed = v.trim() == "1",
            "COOLIFY_COOLD_ACTIVE" => s.coold_active = v.trim() == "1",
            "CORROSION_VERSION" => s.corrosion_version = v.trim().into(),
            "COOLIFY_COOLD_VERSION" => s.coold_version = v.trim().into(),
            "COOLIFY_COOLD_UNIT_HASH" => s.coold_unit_sha256 = v.trim().into(),
            _ => {}
        }
    }
    parse_config(&mut s, &config);
    parse_dump(&mut s, &wg_dump);
    for line in text.lines().filter(|l| l.starts_with("NS=")) {
        if let Some(rest) = line.strip_prefix("NS=") {
            let parts = rest.split('|').collect::<Vec<_>>();
            if parts.len() >= 5 {
                let ns = parts[0].to_string();
                let exists = parts[1] != "missing";
                let subnet = parts[2].parse::<Ipv4Net>().ok();
                let dns = parts[3].trim() == "true";
                let label = parts[4].trim().to_string();
                s.namespaces.insert(
                    ns.clone(),
                    NamespaceServerState {
                        namespace: ns,
                        network_exists: exists,
                        container_subnet: subnet,
                        dns_enabled: dns,
                        label,
                    },
                );
            }
        }
    }
    for ns in namespaces {
        s.namespaces
            .entry(ns.clone())
            .or_insert_with(|| NamespaceServerState {
                namespace: ns.clone(),
                ..Default::default()
            });
    }
    s
}
fn decode_b64(s: &str) -> String {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s.trim())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default()
}
fn parse_config(s: &mut ServerState, cfg: &str) {
    let mut cur: Option<Peer> = None;
    for line in cfg.lines().map(str::trim) {
        if line == "[Peer]" {
            if let Some(p) = cur.take() {
                s.peers.push(p)
            };
            cur = Some(Peer::default());
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        match (cur.as_mut(), k) {
            (None, "Address") => {
                if let Some(ip) = v.split('/').next().and_then(|x| x.parse().ok()) {
                    s.wireguard_mgmt_ip = Some(ip)
                }
            }
            (None, "ListenPort") => s.listen_port = v.parse().unwrap_or(0),
            (Some(p), "PublicKey") => p.public_key = v.into(),
            (Some(p), "Endpoint") => p.endpoint = v.into(),
            (Some(p), "AllowedIPs") => {
                p.allowed_ips = v.split(',').map(|x| x.trim().into()).collect()
            }
            (Some(p), "PersistentKeepalive") => p.persistent_keepalive = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    if let Some(p) = cur {
        s.peers.push(p)
    }
}
fn parse_dump(s: &mut ServerState, dump: &str) {
    if !dump.trim().is_empty() {
        s.active = true;
    }
    let mut lines = dump.lines();
    let _iface = lines.next();
    for (i, line) in lines.enumerate() {
        if let Some(p) = s.peers.get_mut(i) {
            let fields = line.split('\t').collect::<Vec<_>>();
            if fields.len() > 5 {
                p.latest_handshake = fields[5].parse().unwrap_or(0);
            }
        }
    }
}
#[allow(dead_code)]
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
