//! [`Clock`] — the wall-clock seam (RFD §8/§9 portability). All time enters the scheduler through
//! this trait so the same loop runs on EC2 and compiles to `wasm32` for CF Cron Triggers, and so
//! tests use a [`MockClock`] (no wall-clock flake, no live creds).

use std::cell::Cell;

use crate::schedule::Instant;

/// The wall-clock seam: `now()` returns the current instant as epoch **seconds**.
pub trait Clock {
    /// The current instant (epoch seconds, UTC).
    fn now(&self) -> Instant;
}

/// The production clock: reads the system wall clock. Native only — `SystemTime` is not the wasm
/// path's time source (a CF Worker injects the fire time into `tick()` via an explicit instant),
/// so this is gated behind the `native` feature alongside the daemon.
#[cfg(feature = "native")]
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

#[cfg(feature = "native")]
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as Instant)
            .unwrap_or(0)
    }
}

/// A deterministic test clock: `now()` returns a settable instant. wasm-safe (no `SystemTime`),
/// so the golden/plan tests run on every target. Also the shape a CF Worker uses to feed the
/// Cron-Trigger fire time into one `tick()`.
#[derive(Debug)]
pub struct MockClock {
    now: Cell<Instant>,
}

impl MockClock {
    /// A mock clock fixed at `now`.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            now: Cell::new(now),
        }
    }

    /// Set the current instant.
    pub fn set(&self, now: Instant) {
        self.now.set(now);
    }

    /// Advance the current instant by `delta` seconds.
    pub fn advance(&self, delta: Instant) {
        self.now.set(self.now.get() + delta);
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        self.now.get()
    }
}
