//! Self-signed TLS material generation for provisioning.
//!
//! Used by two opt-in hardening paths:
//!   * S1 (flux↔coold channel) — generates a server cert for `flux` and the
//!     matching pin file coold reads (`/etc/coolify/flux.pin`).
//!   * S5 (Corrosion gossip) — generates a shared gossip cert/CA distributed to
//!     every node so gossip can run over mutual TLS instead of plaintext.
//!
//! Both are OPT-IN. Default provisioning generates no TLS material and stays on
//! plaintext-over-WireGuard exactly as before.

use std::net::IpAddr;

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

/// A generated self-signed certificate and its private key, PEM-encoded.
#[derive(Debug, Clone)]
pub struct SelfSignedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate a self-signed certificate for `common_name`, valid for the given
/// subject alternative names. Each SAN is emitted as an IP SAN when it parses as
/// an IP address, otherwise as a DNS SAN. Uses an ECDSA P-256 key via the `ring`
/// backend already present in the workspace.
pub fn generate_self_signed(common_name: &str, sans: &[String]) -> Result<SelfSignedCert> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .context("initialize certificate parameters")?;

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    params.distinguished_name = dn;

    for san in sans {
        let entry = match san.parse::<IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(
                san.clone()
                    .try_into()
                    .with_context(|| format!("invalid DNS SAN {san:?}"))?,
            ),
        };
        params.subject_alt_names.push(entry);
    }

    let key = KeyPair::generate().context("generate key pair")?;
    let cert = params.self_signed(&key).context("self-sign certificate")?;

    Ok(SelfSignedCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_pem_cert_and_key() {
        let material = generate_self_signed("flux", &["100.64.0.1".into(), "flux.internal".into()])
            .expect("cert generation");
        assert!(material.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(material.cert_pem.contains("END CERTIFICATE"));
        assert!(material.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn distinct_invocations_produce_distinct_keys() {
        let a = generate_self_signed("flux", &["100.64.0.1".into()]).unwrap();
        let b = generate_self_signed("flux", &["100.64.0.1".into()]).unwrap();
        assert_ne!(a.key_pem, b.key_pem);
    }
}
