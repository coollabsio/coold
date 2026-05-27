use ipnet::Ipv4Net;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    pub endpoint: String,
    pub public_key: String,
    pub mgmt_ip: Ipv4Addr,
    pub container_subnets: Vec<Ipv4Net>,
}

pub fn allowed_ips_line(p: &PeerConfig) -> String {
    let mut parts = vec![format!("{}/32", p.mgmt_ip)];
    parts.extend(p.container_subnets.iter().map(ToString::to_string));
    parts.join(", ")
}

#[allow(dead_code)]
pub fn render_config(mgmt_ip: Ipv4Addr, listen_port: u16, peers: &[PeerConfig]) -> String {
    let mut s = format!("[Interface]\nAddress = {mgmt_ip}/32\nListenPort = {listen_port}\nPrivateKey = __PRIVKEY__\n");
    for p in peers {
        s.push_str(&format!("\n[Peer]\n# {}\nPublicKey = {}\nAllowedIPs = {}\nEndpoint = {}:{listen_port}\nPersistentKeepalive = 25\n", p.endpoint, p.public_key, allowed_ips_line(p), p.endpoint));
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
        cmd.push_str(&format!("echo \"\"; echo \"[Peer]\"; echo \"# {}\"; echo \"PublicKey = {}\"; echo \"AllowedIPs = {}\"; echo \"Endpoint = {}:{listen_port}\"; echo \"PersistentKeepalive = 25\"; ", p.endpoint, p.public_key, allowed_ips_line(p), p.endpoint));
    }
    cmd.push_str(&format!("}} > /etc/wireguard/{iface}.conf.tmp && chmod 600 /etc/wireguard/{iface}.conf.tmp && mv /etc/wireguard/{iface}.conf.tmp /etc/wireguard/{iface}.conf"));
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
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
