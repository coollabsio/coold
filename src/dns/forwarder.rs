use std::net::SocketAddr;

use hickory_resolver::{
    config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts},
    TokioAsyncResolver,
};

/// Build an async resolver pointed at a single upstream resolver. Used to
/// forward queries outside the coolify zone so containers can still reach
/// public hosts (apt repos, container registries, etc.).
///
/// Caching is hickory-resolver's built-in positive/negative cache (defaults
/// honor record TTLs). No custom cache layer needed.
pub fn build(upstream: SocketAddr) -> TokioAsyncResolver {
    let mut cfg = ResolverConfig::new();
    cfg.add_name_server(NameServerConfig {
        socket_addr: upstream,
        protocol: Protocol::Udp,
        tls_dns_name: None,
        trust_negative_responses: false,
        bind_addr: None,
    });
    cfg.add_name_server(NameServerConfig {
        socket_addr: upstream,
        protocol: Protocol::Tcp,
        tls_dns_name: None,
        trust_negative_responses: false,
        bind_addr: None,
    });

    let mut opts = ResolverOpts::default();
    opts.cache_size = 128;
    opts.preserve_intermediates = false;

    TokioAsyncResolver::tokio(cfg, opts)
}
