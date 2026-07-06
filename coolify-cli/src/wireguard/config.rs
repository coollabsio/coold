use ipnet::Ipv4Net;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    pub endpoint: String,
    pub public_key: String,
    pub mgmt_ip: Ipv4Addr,
    pub container_subnets: Vec<Ipv4Net>,
}

/// S1-adjacent: generate the WireGuard keypair with a private-key file that is
/// never world-readable, even for an instant.
///
/// The previous `wg genkey | tee /etc/wireguard/privatekey` created the file at
/// the process umask (commonly `0022` → `0644`) and only tightened it with a
/// follow-up `chmod 600`, leaving a window in which the private key was
/// world-readable on disk. Running the pipeline under `umask 077` creates
/// `privatekey` (and `publickey`) as `0600`/`0640` from the outset; the trailing
/// `chmod 600` is retained as belt-and-suspenders.
pub fn genkey_command() -> String {
    "mkdir -p /etc/wireguard && (umask 077; wg genkey | tee /etc/wireguard/privatekey | wg pubkey | tee /etc/wireguard/publickey) && chmod 600 /etc/wireguard/privatekey".to_string()
}

pub fn allowed_ips_line(p: &PeerConfig) -> String {
    let mut parts = vec![format!("{}/32", p.mgmt_ip)];
    parts.extend(p.container_subnets.iter().map(ToString::to_string));
    parts.join(", ")
}

fn endpoint_addr(endpoint: &str, listen_port: u16) -> String {
    if endpoint
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        endpoint.to_string()
    } else {
        format!("{endpoint}:{listen_port}")
    }
}

#[allow(dead_code)]
pub fn render_config(mgmt_ip: Ipv4Addr, listen_port: u16, peers: &[PeerConfig]) -> String {
    let mut s = format!("[Interface]\nAddress = {mgmt_ip}/32\nListenPort = {listen_port}\nPrivateKey = __PRIVKEY__\n");
    for p in peers {
        let endpoint = endpoint_addr(&p.endpoint, listen_port);
        s.push_str(&format!("\n[Peer]\n# {}\nPublicKey = {}\nAllowedIPs = {}\nEndpoint = {}\nPersistentKeepalive = 25\n", p.endpoint, p.public_key, allowed_ips_line(p), endpoint));
    }
    s
}

pub fn write_config_command(
    iface: &str,
    mgmt_ip: Ipv4Addr,
    listen_port: u16,
    peers: &[PeerConfig],
) -> String {
    let mut cmd = format!("PRIVKEY=$(cat /etc/wireguard/privatekey) && mkdir -p /etc/wireguard && {{ echo \"[Interface]\"; echo \"Address = {mgmt_ip}/32\"; echo \"ListenPort = {listen_port}\"; echo \"PrivateKey = $PRIVKEY\"; ");
    for p in peers {
        let endpoint = endpoint_addr(&p.endpoint, listen_port);
        cmd.push_str(&format!("echo \"\"; echo \"[Peer]\"; echo \"# {}\"; echo \"PublicKey = {}\"; echo \"AllowedIPs = {}\"; echo \"Endpoint = {}\"; echo \"PersistentKeepalive = 25\"; ", p.endpoint, p.public_key, allowed_ips_line(p), endpoint));
    }
    cmd.push_str(&format!("}} > /etc/wireguard/{iface}.conf.tmp && chmod 600 /etc/wireguard/{iface}.conf.tmp && mv /etc/wireguard/{iface}.conf.tmp /etc/wireguard/{iface}.conf"));
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn genkey_command_closes_umask_window() {
        let cmd = genkey_command();
        assert!(cmd.contains("umask 077"));
        // umask must wrap the genkey pipeline that creates privatekey.
        let umask_at = cmd.find("umask 077").unwrap();
        let tee_at = cmd.find("tee /etc/wireguard/privatekey").unwrap();
        assert!(umask_at < tee_at, "umask must precede key creation");
        assert!(cmd.contains("chmod 600 /etc/wireguard/privatekey"));
    }

    #[test]
    fn allowed_ips_include_mgmt_and_subnets() {
        let p = PeerConfig {
            endpoint: "1.2.3.4".into(),
            public_key: "pk".into(),
            mgmt_ip: "100.64.0.2".parse().unwrap(),
            container_subnets: vec!["10.210.1.0/24".parse().unwrap()],
        };
        assert_eq!(allowed_ips_line(&p), "100.64.0.2/32, 10.210.1.0/24");
    }

    #[test]
    fn render_config_no_peers() {
        let got = render_config("100.64.0.1".parse().unwrap(), 51820, &[]);
        assert!(got.contains("[Interface]"));
        assert!(got.contains("Address = 100.64.0.1/32"));
        assert!(got.contains("ListenPort = 51820"));
        assert!(got.contains("PrivateKey = __PRIVKEY__"));
        assert!(!got.contains("[Peer]"));
    }

    #[test]
    fn render_config_with_multi_namespace_peers() {
        let peers = vec![
            PeerConfig {
                endpoint: "203.0.113.11".into(),
                public_key: "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=".into(),
                mgmt_ip: "100.64.0.1".parse().unwrap(),
                container_subnets: vec!["10.210.1.0/24".parse().unwrap()],
            },
            PeerConfig {
                endpoint: "203.0.113.12".into(),
                public_key: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=".into(),
                mgmt_ip: "100.64.0.2".parse().unwrap(),
                container_subnets: vec![
                    "10.210.2.0/24".parse().unwrap(),
                    "10.220.2.0/24".parse().unwrap(),
                ],
            },
        ];
        let got = render_config("100.64.0.1".parse().unwrap(), 51820, &peers);
        assert_eq!(got.matches("[Peer]").count(), 2);
        assert!(got.contains("Endpoint = 203.0.113.11:51820"));
        assert!(got.contains("AllowedIPs = 100.64.0.1/32, 10.210.1.0/24"));
        assert!(got.contains("PersistentKeepalive = 25"));
        assert!(got.contains("AllowedIPs = 100.64.0.2/32, 10.210.2.0/24, 10.220.2.0/24"));
    }

    #[test]
    fn render_config_keeps_explicit_endpoint_port() {
        let peers = vec![PeerConfig {
            endpoint: "host.lima.internal:51822".into(),
            public_key: "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=".into(),
            mgmt_ip: "100.64.0.2".parse().unwrap(),
            container_subnets: vec![],
        }];
        let got = render_config("100.64.0.1".parse().unwrap(), 51820, &peers);
        assert!(got.contains("Endpoint = host.lima.internal:51822"));
        assert!(!got.contains("host.lima.internal:51822:51820"));
    }

    #[test]
    fn write_config_command_reads_private_key_and_writes_atomically() {
        let peers = vec![PeerConfig {
            endpoint: "203.0.113.11".into(),
            public_key: "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=".into(),
            mgmt_ip: "100.64.0.2".parse().unwrap(),
            container_subnets: vec!["10.210.1.0/24".parse().unwrap()],
        }];
        let cmd = write_config_command("wg0", "100.64.0.1".parse().unwrap(), 51820, &peers);
        for want in [
            "cat /etc/wireguard/privatekey",
            "$PRIVKEY",
            ".conf.tmp",
            "chmod 600 /etc/wireguard/wg0.conf.tmp",
            "mv /etc/wireguard/wg0.conf.tmp /etc/wireguard/wg0.conf",
            "Address = 100.64.0.1/32",
            "100.64.0.2/32, 10.210.1.0/24",
        ] {
            assert!(cmd.contains(want), "missing {want} in:\n{cmd}");
        }
    }
}
