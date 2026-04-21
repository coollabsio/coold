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
//! ## Self-healing bind
//!
//! Netavark creates the Podman bridge on first container attach and **tears
//! it down on last container detach** — so the gateway IP we bind to is not
//! guaranteed to exist when coold starts, and can disappear at runtime (last
//! `podman stop`, manual `ip addr flush`, etc.). [`server::run`] handles this
//! by looping: bind attempts that fail with `EADDRNOTAVAIL`/`EADDRINUSE` back
//! off (1s → 30s cap) and retry until the bridge reappears. Fatal config
//! errors (zone parse, resolver build) propagate up so systemd can restart
//! the daemon. This means the DNS task never silently dies from a missing
//! bridge, and no external sentinel container / boot-time script is needed
//! to keep the bridge alive.
//!
//! The "no DNS during the gap" window is a non-issue: queriers are
//! containers on the bridge, so if the bridge is gone, nobody is asking.
//!
//! ## Port-53 collision defense
//!
//!   1. Bootstrap creates the Podman network with `--disable-dns` so
//!      netavark/aardvark-dns never squats this socket.
//!   2. Bind target is the bridge gateway IP only — never `0.0.0.0`, never
//!      wg0 — so user DNS daemons bound to specific interfaces can coexist.
//!   3. The retry loop above converts transient collisions during netavark
//!      churn (e.g. `EADDRINUSE` briefly) into a retry instead of an exit.

pub mod forwarder;
pub mod resolver;
pub mod server;

pub use server::run;
