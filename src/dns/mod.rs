//! Embedded DNS server for cluster service discovery.
//!
//! Binds UDP+TCP :53 on the Podman bridge gateway IP (e.g. 10.210.X.1) of the
//! local host. Authoritative for the zone configured in
//! [`Config::dns_zone`](crate::config::Config::dns_zone) (default
//! `coolify.internal`). Queries are answered from Corrosion-replicated
//! `service_endpoints`, so the view converges across the whole mesh.
//!
//! Non-authoritative queries (anything outside the zone) are forwarded to
//! `dns_upstream` via [`hickory_resolver`]. Containers typically have
//! `search coolify.internal` in `/etc/resolv.conf`, so `getaddrinfo("foo")`
//! first tries `foo.coolify.internal`; forwarding handles absolute queries
//! like `apt-get`'s `archive.ubuntu.com`.
//!
//! Port 53 conflicts are handled in three layers (see CONTROL_PLANE.md §5):
//!   1. Bootstrap creates the Podman network with `--disable-dns` so
//!      netavark/aardvark-dns never squats this socket.
//!   2. Bind target is the bridge gateway IP only — never `0.0.0.0`, never
//!      wg0 — so user DNS daemons bound to specific interfaces can coexist.
//!   3. Preflight probe fails loud with an actionable error before the
//!      handler is registered.

pub mod forwarder;
pub mod preflight;
pub mod resolver;
pub mod server;

pub use server::run;
