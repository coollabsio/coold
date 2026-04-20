//! Firewall REST API + COOLIFY-ALLOW chain manager.
//!
//! Owns the dynamic allow-rule layer on top of the default-deny scaffold that
//! `coolify init --podman --default-deny` installed on each mesh host. The Go
//! `coolify firewall` CLI writes the same on-disk format (`/etc/coolify/allow.rules`)
//! and uses the same `cid:<12-hex>` rule identity, so both writers produce
//! compatible state.
//!
//! Scope: rule application, snapshot, and REST surface. Metadata
//! (audit/RBAC/owners) is the central Coolify DB's job — coold holds tuples
//! only. See `CONTROL_PLANE.md §3` in the CLI repo for the split.
//!
//! Layout mirrors `dns/`: pure logic (`rule`), side-effecting boundary
//! (`store`), transport (`api`), and a bind-serve loop (`server`) wired into
//! `sync::run` as a tokio task.

pub mod api;
pub mod rule;
pub mod server;
pub mod store;

pub use server::run;
