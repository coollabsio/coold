use ipnet::Ipv4Net;

pub const ALLOW_RULES_PATH: &str = "/etc/coolify/allow.rules";
pub const BRIDGE_TABLE_NAME: &str = "coolify_bridge";
pub const BRIDGE_ALLOW_RULES_PATH: &str = "/etc/coolify/allow.nft";
pub const BRIDGE_SCAFFOLD_PATH: &str = "/etc/coolify/bridge-fw.nft";
const FIREWALL_UNIT_PATH: &str = "/etc/systemd/system/coolify-mesh-fw.service";
const FIREWALL_SERVICE_NAME: &str = "coolify-mesh-fw.service";

pub fn firewall_service_unit(
    iface: &str,
    _namespaces: &[String],
    container_subnets: &[Ipv4Net],
    default_deny: bool,
) -> String {
    let mut b = format!("[Unit]\nDescription=Coolify mesh firewall rules\nAfter=wg-quick@{iface}.service network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\n\n");
    for sn in container_subnets {
        b.push_str(&format!("ExecStart=/bin/sh -c \"/usr/sbin/iptables -t nat -C POSTROUTING -s {sn} -o {iface} -j RETURN 2>/dev/null || /usr/sbin/iptables -t nat -I POSTROUTING -s {sn} -o {iface} -j RETURN\"\n"));
    }
    if default_deny {
        b.push_str("# Remove blanket ACCEPT from prior mode-A run.\n");
        for sn in container_subnets {
            b.push_str(&format!("ExecStart=/bin/sh -c \"/usr/sbin/iptables -D FORWARD -s {sn} -j ACCEPT 2>/dev/null || true\"\nExecStart=/bin/sh -c \"/usr/sbin/iptables -D FORWARD -d {sn} -j ACCEPT 2>/dev/null || true\"\n"));
        }
        b.push_str(&format!("\nExecStart=/bin/sh -c \"/usr/sbin/iptables -N COOLIFY-ALLOW 2>/dev/null || true\"\nExecStart=/bin/sh -c \"/usr/sbin/iptables -N COOLIFY-INTRA 2>/dev/null || true\"\nExecStart=/usr/sbin/iptables -F COOLIFY-INTRA\nExecStart=/usr/sbin/iptables -A COOLIFY-INTRA -j COOLIFY-ALLOW\nExecStart=/usr/sbin/iptables -A COOLIFY-INTRA -j DROP\nExecStart=/bin/sh -c \"[ -s {ALLOW_RULES_PATH} ] && /usr/sbin/iptables -F COOLIFY-ALLOW && /usr/sbin/iptables-restore --noflush < {ALLOW_RULES_PATH} || true\"\nExecStart=/bin/sh -c \"/usr/sbin/iptables -C FORWARD -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT 2>/dev/null || /usr/sbin/iptables -I FORWARD 1 -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT\"\n"));
        for sn in container_subnets {
            b.push_str(&format!("ExecStart=/bin/sh -c \"/usr/sbin/iptables -C FORWARD -d {sn} -j COOLIFY-INTRA 2>/dev/null || /usr/sbin/iptables -A FORWARD -d {sn} -j COOLIFY-INTRA\"\nExecStart=/bin/sh -c \"/usr/sbin/iptables -C FORWARD -s {sn} -j COOLIFY-INTRA 2>/dev/null || /usr/sbin/iptables -A FORWARD -s {sn} -j COOLIFY-INTRA\"\n"));
        }
        b.push_str(&format!("ExecStart=/bin/sh -c \"nft list table bridge {BRIDGE_TABLE_NAME} >/dev/null 2>&1 || nft add table bridge {BRIDGE_TABLE_NAME}\"\nExecStart=/bin/sh -c \"nft add chain bridge {BRIDGE_TABLE_NAME} coolify_allow '{{ }}' 2>/dev/null || true\"\nExecStart=/bin/sh -c \"nft delete chain bridge {BRIDGE_TABLE_NAME} forward 2>/dev/null || true\"\nExecStart=/bin/sh -c \"nft delete chain bridge {BRIDGE_TABLE_NAME} coolify_intra 2>/dev/null || true\"\nExecStart=/bin/sh -c \"nft -f {BRIDGE_SCAFFOLD_PATH}\"\nExecStart=/bin/sh -c \"[ -s {BRIDGE_ALLOW_RULES_PATH} ] && nft -f {BRIDGE_ALLOW_RULES_PATH} || true\"\n"));
    } else {
        b.push_str(&format!("# Tear down default-deny scaffold from prior run.\nExecStart=/bin/sh -c \"/usr/sbin/iptables -F COOLIFY-INTRA 2>/dev/null || true\"\nExecStart=/bin/sh -c \"/usr/sbin/iptables -X COOLIFY-INTRA 2>/dev/null || true\"\nExecStart=/bin/sh -c \"nft delete table bridge {BRIDGE_TABLE_NAME} 2>/dev/null || true\"\n"));
        for sn in container_subnets {
            b.push_str(&format!("ExecStart=/bin/sh -c \"/usr/sbin/iptables -C FORWARD -s {sn} -j ACCEPT 2>/dev/null || /usr/sbin/iptables -I FORWARD -s {sn} -j ACCEPT\"\nExecStart=/bin/sh -c \"/usr/sbin/iptables -C FORWARD -d {sn} -j ACCEPT 2>/dev/null || /usr/sbin/iptables -I FORWARD -d {sn} -j ACCEPT\"\n"));
        }
    }
    b.push_str("\n[Install]\nWantedBy=multi-user.target\n");
    b
}

pub fn render_bridge_scaffold(subnets: &[Ipv4Net]) -> String {
    let mut s = subnets.iter().map(ToString::to_string).collect::<Vec<_>>();
    s.sort();
    let set = format!("{{ {} }}", s.join(", "));
    format!("# Managed by coolify — do not edit manually.\nadd table bridge {BRIDGE_TABLE_NAME}\nadd chain bridge {BRIDGE_TABLE_NAME} coolify_intra\nflush chain bridge {BRIDGE_TABLE_NAME} coolify_intra\nadd rule bridge {BRIDGE_TABLE_NAME} coolify_intra jump coolify_allow\nadd rule bridge {BRIDGE_TABLE_NAME} coolify_intra drop\nadd chain bridge {BRIDGE_TABLE_NAME} forward {{ type filter hook forward priority -200; policy accept; }}\nflush chain bridge {BRIDGE_TABLE_NAME} forward\nadd rule bridge {BRIDGE_TABLE_NAME} forward meta protocol != ip accept\nadd rule bridge {BRIDGE_TABLE_NAME} forward ct state established,related accept\nadd rule bridge {BRIDGE_TABLE_NAME} forward ip saddr {set} jump coolify_intra\nadd rule bridge {BRIDGE_TABLE_NAME} forward ip daddr {set} jump coolify_intra\n")
}

pub fn install_firewall_command(
    iface: &str,
    namespaces: &[String],
    subnets: &[Ipv4Net],
    default_deny: bool,
) -> String {
    let unit = firewall_service_unit(iface, namespaces, subnets, default_deny);
    let mut cmd = format!("cat > {FIREWALL_UNIT_PATH}.tmp <<'COOLIFY_FW_EOF'\n{unit}COOLIFY_FW_EOF\nmv {FIREWALL_UNIT_PATH}.tmp {FIREWALL_UNIT_PATH} && mkdir -p /etc/coolify && ");
    if default_deny {
        let scaffold = render_bridge_scaffold(subnets);
        cmd.push_str(&format!("cat > {BRIDGE_SCAFFOLD_PATH}.tmp <<'COOLIFY_BR_EOF'\n{scaffold}COOLIFY_BR_EOF\nmv {BRIDGE_SCAFFOLD_PATH}.tmp {BRIDGE_SCAFFOLD_PATH} && "));
    } else {
        cmd.push_str(&format!("rm -f {BRIDGE_SCAFFOLD_PATH} && "));
    }
    cmd.push_str(&format!("systemctl daemon-reload && systemctl enable {FIREWALL_SERVICE_NAME} && systemctl restart {FIREWALL_SERVICE_NAME}"));
    cmd
}
