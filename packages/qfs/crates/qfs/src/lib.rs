#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! `qfs` library facet — the composition-root code shared by the `qfs` binary (`main.rs`)
//! and the `xtask` build tooling.
//!
//! The `qfs` crate is primarily the **single binary** (RFD-0001 §9). It also exposes this
//! thin library facet so two consumers can reuse the binary's *own* registries instead of
//! re-deriving them:
//!
//! - `main.rs` wires [`describe::describe_registry`], the serve launcher, the shell, etc.
//! - `xtask` (the build tool, never shipped) calls [`docs::gen_docs`], which renders the
//!   reference docs from [`catalog::driver_catalog`] — built by walking the **same**
//!   describe registry the binary ships. That is the anti-drift guarantee (t40): the docs
//!   are generated from the binary's live introspection surface, never authored twice.
//!
//! The modules that wire the runtime-coupled serve/shell/host facets (`serve`, `shell`,
//! `host`, `job`, `watchtower`, `serve_builtins`) stay `pub(crate)`-only in spirit but are
//! exposed here as modules so `main.rs` can reference them through the crate root; nothing
//! outside this crate links the binary as a library except `xtask`, which touches only the
//! pure [`catalog`]/[`docs`]/[`version`] surface (no runtime, no creds, no I/O).

pub mod audit;
pub mod billing;
pub mod broker;
pub mod catalog;
pub mod claude;
pub mod clients;
pub mod commit;
pub mod connection;
pub mod dashboard;
pub mod describe;
pub mod directory;
pub mod docs;
pub mod e2e_store;
pub mod fs;
pub mod git;
pub mod google;
pub mod host;
pub mod identity;
pub mod invite;
pub mod job;
pub mod mcp;
pub mod oauth;
pub mod objstore;
pub mod read_facets;
pub mod secret_store;
pub mod serve;
pub mod serve_builtins;
pub mod session;
pub mod shared_connection;
pub mod shell;
pub mod sql;
pub mod store;
pub mod sys;
pub mod telemetry;
pub mod transport;
pub mod tunnel;
pub mod version;
pub mod watchtower;
pub mod worm;

/// A process-global lock serializing the **test-only** mutation of shared environment variables
/// (`XDG_CONFIG_HOME` and friends) across the crate's test modules.
///
/// The config-home resolvers (`store.rs` and the OAuth flow store in `oauth.rs`) read
/// `XDG_CONFIG_HOME` process-globally. Both modules' tests set + restore it; previously each owned a
/// *module-local* `ENV_LOCK`, which serialized within a module but NOT across modules — so a
/// `store.rs` test setting `XDG_CONFIG_HOME` could corrupt an in-flight `oauth.rs` test's config home
/// (and vice-versa), panicking it and poisoning that module's lock (a cascade of false failures under
/// the parallel harness). Sharing ONE lock makes every env-mutating test across the crate mutually
/// exclusive, so the suite is deterministic regardless of thread scheduling.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
