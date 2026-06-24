//! The `/server/...` mount point (RFD-0001 §8).
//!
//! ## The server IS a driver (fidelity guard G4, boundary B5)
//! §8 is explicit: the server is **a driver**, not a privileged subsystem. Its endpoints,
//! triggers, jobs, views, policies, and webhooks are **data** managed via qfs (`CREATE …`
//! forms are sugar over `INSERT INTO /server/...`). The actual [`Driver`](qfs_core::Driver)
//! impl over `/server/...` is [`crate::driver::ServerDriver`], registered into a
//! `MountRegistry` like any other driver — the server is never a bespoke entrypoint that
//! bypasses the mount registry.

/// The reserved mount point for the server-as-a-driver (RFD-0001 §8). Single source of
/// truth, re-used by [`crate::driver`].
pub use crate::driver::SERVER_MOUNT;
