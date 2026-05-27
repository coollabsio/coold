use anyhow::{bail, Result};
use ipnet::Ipv4Net;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub host: String,
    pub reason: String,
}

fn ip_to_u32(ip: Ipv4Addr) -> u32 {
    u32::from(ip)
}
fn u32_to_ip(v: u32) -> Ipv4Addr {
    Ipv4Addr::from(v)
}

pub fn machine_ip(net: Ipv4Net) -> Ipv4Addr {
    u32_to_ip(ip_to_u32(net.network()) + 1)
}

pub fn allocate(
    pool: Ipv4Net,
    prefix: u8,
    existing: &BTreeMap<String, Ipv4Net>,
    hosts: &[String],
) -> Result<(BTreeMap<String, Ipv4Net>, Vec<Warning>)> {
    if prefix < pool.prefix_len() || prefix > 32 {
        bail!("prefix /{prefix} is not valid inside pool {pool}");
    }
    let mut seen_hosts = BTreeSet::new();
    for h in hosts {
        if !seen_hosts.insert(h) {
            bail!("duplicate host in --servers: {h}");
        }
    }
    let base = ip_to_u32(pool.network());
    let broadcast = ip_to_u32(pool.broadcast());
    let step = 1u32.checked_shl((32 - prefix) as u32).unwrap_or(1);
    let mut result = BTreeMap::new();
    let mut used = BTreeSet::new();
    let mut warnings = vec![];
    for (host, net) in existing {
        if !hosts.contains(host) {
            continue;
        }
        if net.prefix_len() != prefix || !pool.contains(&net.network()) {
            warnings.push(Warning {
                host: host.clone(),
                reason: format!(
                    "existing subnet {net} is not a /{prefix} inside pool {pool}, reassigning"
                ),
            });
            continue;
        }
        let n = ip_to_u32(net.network());
        if !used.insert(n) {
            warnings.push(Warning {
                host: host.clone(),
                reason: format!("duplicate subnet {net}, reassigning"),
            });
            continue;
        }
        result.insert(host.clone(), *net);
    }
    for host in hosts {
        if result.contains_key(host) {
            continue;
        }
        let mut n = base;
        loop {
            if n > broadcast {
                bail!("pool {pool} is exhausted (no free /{prefix} subnets)");
            }
            if !used.contains(&n) {
                break;
            }
            n = n.saturating_add(step);
        }
        used.insert(n);
        result.insert(host.clone(), Ipv4Net::new(u32_to_ip(n), prefix)?);
    }
    Ok((result, warnings))
}

pub fn allocate_mgmt_ips(
    pool: Ipv4Net,
    existing: &BTreeMap<String, Ipv4Addr>,
    hosts: &[String],
) -> Result<(BTreeMap<String, Ipv4Addr>, Vec<Warning>)> {
    let mut seen_hosts = BTreeSet::new();
    for h in hosts {
        if !seen_hosts.insert(h) {
            bail!("duplicate host in --servers: {h}");
        }
    }
    let base = ip_to_u32(pool.network());
    let broadcast = ip_to_u32(pool.broadcast());
    let mut result = BTreeMap::new();
    let mut used = BTreeSet::new();
    let mut warnings = vec![];
    for (host, ip) in existing {
        if !hosts.contains(host) {
            continue;
        }
        let raw = ip_to_u32(*ip);
        if raw == base || raw == broadcast || !pool.contains(ip) {
            warnings.push(Warning {
                host: host.clone(),
                reason: format!(
                    "existing mgmt IP {ip} is not usable inside pool {pool}, reassigning"
                ),
            });
            continue;
        }
        if !used.insert(raw) {
            warnings.push(Warning {
                host: host.clone(),
                reason: format!("duplicate mgmt IP {ip}, reassigning"),
            });
            continue;
        }
        result.insert(host.clone(), *ip);
    }
    for host in hosts {
        if result.contains_key(host) {
            continue;
        }
        let mut n = base.saturating_add(1);
        loop {
            if n >= broadcast {
                bail!("pool {pool} is exhausted (no free management IPs)");
            }
            if !used.contains(&n) {
                break;
            }
            n = n.saturating_add(1);
        }
        used.insert(n);
        result.insert(host.clone(), u32_to_ip(n));
    }
    Ok((result, warnings))
}

#[allow(clippy::type_complexity)]
pub fn allocate_namespaced(
    pool: Ipv4Net,
    prefix: u8,
    existing: &BTreeMap<String, BTreeMap<String, Ipv4Net>>,
    namespaces: &[String],
    hosts: &[String],
) -> Result<(BTreeMap<String, BTreeMap<String, Ipv4Net>>, Vec<Warning>)> {
    let mut seen_ns = BTreeSet::new();
    for ns in namespaces {
        if !seen_ns.insert(ns) {
            bail!("duplicate namespace in --namespaces: {ns}");
        }
    }
    let mut all_existing = BTreeMap::new();
    for ns in namespaces {
        for h in hosts {
            if let Some(n) = existing.get(ns).and_then(|m| m.get(h)) {
                all_existing.insert(format!("{ns}/{h}"), *n);
            }
        }
    }
    let keys = namespaces
        .iter()
        .flat_map(|ns| hosts.iter().map(move |h| format!("{ns}/{h}")))
        .collect::<Vec<_>>();
    let (flat, warnings) = allocate(pool, prefix, &all_existing, &keys)?;
    let mut out: BTreeMap<String, BTreeMap<String, Ipv4Net>> = BTreeMap::new();
    for ns in namespaces {
        for h in hosts {
            let key = format!("{ns}/{h}");
            out.entry(ns.clone())
                .or_default()
                .insert(h.clone(), flat[&key]);
        }
    }
    Ok((out, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hosts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).into()).collect()
    }

    #[test]
    fn allocates_stable_subnets() {
        let pool: Ipv4Net = "10.210.0.0/16".parse().unwrap();
        let hosts = vec!["a".into(), "b".into()];
        let (got, _) = allocate(pool, 24, &BTreeMap::new(), &hosts).unwrap();
        assert_eq!(got["a"].to_string(), "10.210.0.0/24");
        assert_eq!(got["b"].to_string(), "10.210.1.0/24");
    }

    #[test]
    fn machine_ip_uses_first_host_address() {
        for (subnet, want) in [
            ("10.210.0.0/24", "10.210.0.1"),
            ("10.210.5.0/24", "10.210.5.1"),
            ("192.168.0.0/24", "192.168.0.1"),
        ] {
            assert_eq!(machine_ip(subnet.parse().unwrap()).to_string(), want);
        }
    }

    #[test]
    fn allocate_mgmt_ips_starts_after_pool_network_and_reuses_existing() {
        let pool: Ipv4Net = "100.64.0.0/16".parse().unwrap();
        let (got, warnings) =
            allocate_mgmt_ips(pool, &BTreeMap::new(), &hosts(&["h1", "h2"])).expect("allocate");
        assert!(warnings.is_empty());
        assert_eq!(got["h1"].to_string(), "100.64.0.1");
        assert_eq!(got["h2"].to_string(), "100.64.0.2");

        let existing = BTreeMap::from([("h1".into(), "100.64.0.42".parse().unwrap())]);
        let (got, warnings) =
            allocate_mgmt_ips(pool, &existing, &hosts(&["h1", "h2"])).expect("allocate");
        assert!(warnings.is_empty());
        assert_eq!(got["h1"].to_string(), "100.64.0.42");
        assert_eq!(got["h2"].to_string(), "100.64.0.1");
    }

    #[test]
    fn allocate_mgmt_ips_rejects_pool_network_broadcast_and_duplicates() {
        let pool: Ipv4Net = "100.64.0.0/16".parse().unwrap();
        let existing = BTreeMap::from([
            ("hN".into(), "100.64.0.0".parse().unwrap()),
            ("hB".into(), "100.64.255.255".parse().unwrap()),
            ("h1".into(), "100.64.0.42".parse().unwrap()),
            ("h2".into(), "100.64.0.42".parse().unwrap()),
        ]);
        let (got, warnings) =
            allocate_mgmt_ips(pool, &existing, &hosts(&["hN", "hB", "h1", "h2"])).unwrap();
        assert_eq!(warnings.len(), 3);
        assert_eq!(got["h1"].to_string(), "100.64.0.42");
        assert_ne!(got["hN"].to_string(), "100.64.0.0");
        assert_ne!(got["hB"].to_string(), "100.64.255.255");
        assert_ne!(got["h2"].to_string(), "100.64.0.42");
    }

    #[test]
    fn allocate_fills_gaps_and_warns_on_duplicate_or_invalid_existing() {
        let pool: Ipv4Net = "10.210.0.0/16".parse().unwrap();
        let existing = BTreeMap::from([
            ("h1".into(), "10.210.0.0/24".parse().unwrap()),
            ("h2".into(), "10.210.2.0/24".parse().unwrap()),
        ]);
        let (got, warnings) = allocate(pool, 24, &existing, &hosts(&["h1", "h2", "h3"])).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(got["h3"].to_string(), "10.210.1.0/24");

        let duplicate = BTreeMap::from([
            ("ha".into(), "10.210.5.0/24".parse().unwrap()),
            ("hb".into(), "10.210.5.0/24".parse().unwrap()),
        ]);
        let (got, warnings) = allocate(pool, 24, &duplicate, &hosts(&["ha", "hb"])).unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].host, "hb");
        assert!(warnings[0].reason.contains("duplicate subnet"));
        assert_eq!(got["ha"].to_string(), "10.210.5.0/24");
        assert_ne!(got["hb"].to_string(), "10.210.5.0/24");

        let invalid = BTreeMap::from([("h1".into(), "192.168.0.0/24".parse().unwrap())]);
        let (got, warnings) = allocate(pool, 24, &invalid, &hosts(&["h1"])).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].reason.contains("not a /24 inside pool"));
        assert!(pool.contains(&got["h1"].network()));
    }

    #[test]
    fn allocate_rejects_duplicate_hosts_and_exhaustion() {
        let pool: Ipv4Net = "10.210.0.0/16".parse().unwrap();
        let err = allocate(pool, 24, &BTreeMap::new(), &hosts(&["h1", "h1"])).unwrap_err();
        assert!(err.to_string().contains("duplicate host"));

        let small: Ipv4Net = "10.0.0.0/28".parse().unwrap();
        let err = allocate(small, 28, &BTreeMap::new(), &hosts(&["h1", "h2"])).unwrap_err();
        assert!(err.to_string().contains("exhausted"));
    }

    #[test]
    fn allocate_namespaced_detects_duplicate_namespaces() {
        let pool: Ipv4Net = "10.210.0.0/16".parse().unwrap();
        let err = allocate_namespaced(
            pool,
            24,
            &BTreeMap::new(),
            &["default".into(), "default".into()],
            &hosts(&["h1"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate namespace"));
    }
}
