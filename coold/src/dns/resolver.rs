use std::{net::IpAddr, str::FromStr, sync::Arc};

use async_trait::async_trait;
use hickory_proto::{
    op::{Header, MessageType, OpCode, ResponseCode},
    rr::{rdata::A, LowerName, Name, RData, Record, RecordType},
};
use hickory_resolver::TokioAsyncResolver;
use hickory_server::{
    authority::MessageResponseBuilder,
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo},
};
use tracing::{debug, warn};

/// Positive-answer TTL for in-zone A records (seconds). Matches
/// CONTROL_PLANE.md §TTL: 5s is the convergence-vs-chatter sweet spot.
const POSITIVE_TTL: u32 = 5;

/// Backend that resolves `(container_name, namespace)` to IPv4 addresses.
/// Abstracted as a trait so tests can substitute a deterministic source
/// without spinning up Corrosion.
#[async_trait]
pub trait EndpointLookup: Send + Sync + 'static {
    async fn lookup(&self, container_name: &str, namespace: &str) -> anyhow::Result<Vec<IpAddr>>;
}

/// Production backend: consults the local Corrosion HTTP API for endpoints
/// matching `(container_name, namespace)` across the mesh.
pub struct CorrosionBackend {
    client: crate::corrosion::CorrosionClient,
}

impl CorrosionBackend {
    pub fn new(client: crate::corrosion::CorrosionClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl EndpointLookup for CorrosionBackend {
    async fn lookup(&self, container_name: &str, namespace: &str) -> anyhow::Result<Vec<IpAddr>> {
        let ips = self
            .client
            .query_ips_by_name(container_name, namespace)
            .await?;
        Ok(ips
            .into_iter()
            .filter_map(|s| IpAddr::from_str(&s).ok())
            .collect())
    }
}

/// `hickory-server` handler that owns the zone logic. Authoritative for
/// `zone` (typically `coolify.internal.`); everything else is forwarded via
/// `upstream`.
pub struct CoolifyResolver {
    backend: Arc<dyn EndpointLookup>,
    zone: LowerName,
    upstream: TokioAsyncResolver,
}

impl CoolifyResolver {
    pub fn new(
        backend: Arc<dyn EndpointLookup>,
        zone: &str,
        upstream: TokioAsyncResolver,
    ) -> anyhow::Result<Self> {
        let mut fqdn = Name::from_utf8(zone)?;
        fqdn.set_fqdn(true);
        Ok(Self {
            backend,
            zone: LowerName::from(fqdn),
            upstream,
        })
    }

    /// Extract `(container, namespace)` from an in-zone query.
    ///
    /// Expected shape is `<container>.<namespace>.<zone>`, e.g.
    /// `web.default.coolify.internal.` → `Some(("web", "default"))`.
    /// Anything shorter (bare `<name>.<zone>`, zone apex) returns `None` so
    /// the resolver answers NXDOMAIN — callers must fully qualify.
    fn container_and_namespace<'a>(&self, query_name: &'a LowerName) -> Option<(String, String)> {
        if !self.zone.zone_of(query_name) {
            return None;
        }
        let qn = Name::from(query_name.clone());
        let zn = Name::from(self.zone.clone());
        let extra = qn.num_labels().saturating_sub(zn.num_labels());
        // Need at least two labels above the zone (<name>.<namespace>.<zone>).
        if extra < 2 {
            return None;
        }
        // Two leftmost labels: container, then namespace.
        let mut labels = qn.iter();
        let container = labels
            .next()
            .map(|b| String::from_utf8_lossy(b).to_lowercase())?;
        let namespace = labels
            .next()
            .map(|b| String::from_utf8_lossy(b).to_lowercase())?;
        if container.is_empty() || namespace.is_empty() {
            return None;
        }
        Some((container, namespace))
    }

    async fn answer_internal<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response: R,
        container: &str,
        namespace: &str,
    ) -> ResponseInfo {
        let ips = match self.backend.lookup(container, namespace).await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    container = %container,
                    namespace = %namespace,
                    error = %e,
                    "backend lookup failed",
                );
                return send_code(request, response, ResponseCode::ServFail).await;
            }
        };

        let q = request.query();
        if q.query_type() != RecordType::A {
            // In-zone but not A: NODATA (empty answer, NOERROR).
            return send_empty(request, response).await;
        }

        if ips.is_empty() {
            return send_code(request, response, ResponseCode::NXDomain).await;
        }

        let name: Name = q.name().into();
        let records: Vec<Record> = ips
            .into_iter()
            .filter_map(|ip| match ip {
                IpAddr::V4(v4) => Some(Record::from_rdata(
                    name.clone(),
                    POSITIVE_TTL,
                    RData::A(A(v4)),
                )),
                IpAddr::V6(_) => None, // IPv4-only v1.
            })
            .collect();

        if records.is_empty() {
            return send_empty(request, response).await;
        }

        let builder = MessageResponseBuilder::from_message_request(request);
        let mut header = Header::response_from_request(request.header());
        header.set_authoritative(true);
        header.set_response_code(ResponseCode::NoError);
        header.set_message_type(MessageType::Response);
        header.set_op_code(OpCode::Query);

        let msg = builder.build(header, records.iter(), &[], &[], &[]);
        match response.send_response(msg).await {
            Ok(info) => info,
            Err(e) => {
                warn!(error = %e, "send_response failed");
                fallback_info(request)
            }
        }
    }

    /// In-zone query that did not match the `<name>.<ns>.<zone>` shape.
    /// Returns NXDOMAIN authoritatively so clients don't fall through to
    /// search-domain guessing against the upstream resolver.
    async fn answer_in_zone_nxdomain<R: ResponseHandler>(
        &self,
        request: &Request,
        response: R,
    ) -> ResponseInfo {
        send_code(request, response, ResponseCode::NXDomain).await
    }

    async fn answer_forwarded<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response: R,
    ) -> ResponseInfo {
        let q = request.query();
        let name: Name = q.name().into();
        let rtype = q.query_type();

        let lookup = match self.upstream.lookup(name.clone(), rtype).await {
            Ok(l) => l,
            Err(e) => {
                debug!(query = %name, rtype = ?rtype, error = %e, "upstream lookup failed");
                return send_code(request, response, ResponseCode::ServFail).await;
            }
        };

        let records: Vec<Record> = lookup.records().to_vec();
        let builder = MessageResponseBuilder::from_message_request(request);
        let mut header = Header::response_from_request(request.header());
        header.set_response_code(ResponseCode::NoError);
        header.set_message_type(MessageType::Response);
        header.set_op_code(OpCode::Query);

        let msg = builder.build(header, records.iter(), &[], &[], &[]);
        match response.send_response(msg).await {
            Ok(info) => info,
            Err(e) => {
                warn!(error = %e, "send_response failed");
                fallback_info(request)
            }
        }
    }
}

#[async_trait]
impl RequestHandler for CoolifyResolver {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        response: R,
    ) -> ResponseInfo {
        if request.op_code() != OpCode::Query || request.message_type() != MessageType::Query {
            return send_code(request, response, ResponseCode::Refused).await;
        }

        let qname = request.query().name().clone();
        if self.zone.zone_of(&qname) {
            match self.container_and_namespace(&qname) {
                Some((container, namespace)) => {
                    self.answer_internal(request, response, &container, &namespace)
                        .await
                }
                None => self.answer_in_zone_nxdomain(request, response).await,
            }
        } else {
            self.answer_forwarded(request, response).await
        }
    }
}

async fn send_code<R: ResponseHandler>(
    request: &Request,
    mut response: R,
    code: ResponseCode,
) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let msg = builder.error_msg(request.header(), code);
    match response.send_response(msg).await {
        Ok(info) => info,
        Err(_) => fallback_info(request),
    }
}

async fn send_empty<R: ResponseHandler>(request: &Request, mut response: R) -> ResponseInfo {
    let builder = MessageResponseBuilder::from_message_request(request);
    let mut header = Header::response_from_request(request.header());
    header.set_authoritative(true);
    header.set_response_code(ResponseCode::NoError);
    header.set_message_type(MessageType::Response);
    header.set_op_code(OpCode::Query);
    let msg = builder.build(header, &[], &[], &[], &[]);
    match response.send_response(msg).await {
        Ok(info) => info,
        Err(_) => fallback_info(request),
    }
}

fn fallback_info(request: &Request) -> ResponseInfo {
    // When we can't actually send bytes, still surface a response header so
    // hickory-server's metrics/logs don't panic. ServFail is the safest
    // sentinel.
    let mut header = Header::response_from_request(request.header());
    header.set_response_code(ResponseCode::ServFail);
    ResponseInfo::from(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    struct FakeBackend {
        records: std::collections::HashMap<(String, String), Vec<IpAddr>>,
    }

    #[async_trait]
    impl EndpointLookup for FakeBackend {
        async fn lookup(
            &self,
            container_name: &str,
            namespace: &str,
        ) -> anyhow::Result<Vec<IpAddr>> {
            Ok(self
                .records
                .get(&(container_name.to_string(), namespace.to_string()))
                .cloned()
                .unwrap_or_default())
        }
    }

    fn build_resolver(backend: FakeBackend) -> CoolifyResolver {
        let upstream = crate::dns::forwarder::build("127.0.0.1:53".parse().unwrap());
        CoolifyResolver::new(Arc::new(backend), "coolify.internal.", upstream).unwrap()
    }

    #[tokio::test]
    async fn extracts_namespace_qualified_name() {
        let resolver = build_resolver(FakeBackend {
            records: Default::default(),
        });
        let name = LowerName::from(Name::from_utf8("web.default.coolify.internal.").unwrap());
        let got = resolver.container_and_namespace(&name);
        assert_eq!(
            got.as_ref().map(|(c, n)| (c.as_str(), n.as_str())),
            Some(("web", "default"))
        );
    }

    #[tokio::test]
    async fn bare_name_rejected() {
        let resolver = build_resolver(FakeBackend {
            records: Default::default(),
        });
        let name = LowerName::from(Name::from_utf8("web.coolify.internal.").unwrap());
        assert!(resolver.container_and_namespace(&name).is_none());
    }

    #[tokio::test]
    async fn zone_apex_rejected() {
        let resolver = build_resolver(FakeBackend {
            records: Default::default(),
        });
        let name = LowerName::from(Name::from_utf8("coolify.internal.").unwrap());
        assert!(resolver.container_and_namespace(&name).is_none());
    }

    #[tokio::test]
    async fn out_of_zone_rejected() {
        let resolver = build_resolver(FakeBackend {
            records: Default::default(),
        });
        let name = LowerName::from(Name::from_utf8("example.com.").unwrap());
        assert!(resolver.container_and_namespace(&name).is_none());
    }

    #[tokio::test]
    async fn backend_scopes_by_namespace() {
        let mut records = std::collections::HashMap::new();
        records.insert(
            ("web".into(), "default".into()),
            vec![IpAddr::V4(Ipv4Addr::new(10, 210, 0, 2))],
        );
        records.insert(
            ("web".into(), "alpha".into()),
            vec![IpAddr::V4(Ipv4Addr::new(10, 220, 0, 2))],
        );
        let backend = FakeBackend { records };
        let def = backend.lookup("web", "default").await.unwrap();
        let alp = backend.lookup("web", "alpha").await.unwrap();
        assert_eq!(def, vec![IpAddr::V4(Ipv4Addr::new(10, 210, 0, 2))]);
        assert_eq!(alp, vec![IpAddr::V4(Ipv4Addr::new(10, 220, 0, 2))]);
    }
}
