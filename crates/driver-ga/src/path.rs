//! [`GaPath`] — the parse of a cfs [`Path`](cfs_driver::Path) into the concrete GA4 node it
//! names (RFD-0001 §5). Google Analytics maps onto the **relational archetype**: a GA4 property
//! is a queryable relation of **metrics grouped by dimensions over a date range**, addressed by
//! its numeric property id.
//!
//! ## Addressing
//! - `/ga` — the virtual root (no property selected; lists nothing queryable on its own).
//! - `/ga/<propertyId>` — a GA4 property's **core report** relation (`properties.runReport`).
//! - `/ga/<propertyId>/realtime` — the property's **realtime report** relation
//!   (`properties.runRealtimeReport`, last ~30 min, a restricted dimension/metric catalog).
//!
//! The property id is the GA4 numeric id (e.g. `123456789`); it is threaded into the
//! `properties/<id>:runReport` resource path and into the `Secrets` account selector for
//! multi-account credential resolution. Pure parsing only — no I/O, no vendor type crosses.

use cfs_driver::Path;

use crate::error::GaError;

/// The mount this driver answers for. The virtual root carries no property; a child segment
/// selects the GA4 property.
pub const MOUNT: &str = "/ga";

/// The reserved trailing segment selecting the realtime report surface
/// (`properties.runRealtimeReport`).
pub const REALTIME_SEGMENT: &str = "realtime";

/// A parsed GA4 address — what a `/ga/...` path resolves to. Owned, vendor-free. The
/// introspective methods and the compiler branch on this.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GaPath {
    /// `/ga` — the virtual root (no property selected).
    Root,
    /// `/ga/<propertyId>` — a property's core report relation (`runReport`).
    Property {
        /// The GA4 numeric property id (e.g. `123456789`).
        property_id: String,
    },
    /// `/ga/<propertyId>/realtime` — the property's realtime report relation
    /// (`runRealtimeReport`).
    Realtime {
        /// The GA4 numeric property id.
        property_id: String,
    },
}

impl GaPath {
    /// Parse a driver [`Path`] string into a [`GaPath`].
    ///
    /// # Errors
    /// [`GaError::InvalidPath`] if the path is not under `/ga`, names an empty property id, or
    /// carries an unexpected trailing segment.
    pub fn parse(path: &Path) -> Result<Self, GaError> {
        Self::parse_str(path.as_str())
    }

    /// Parse a raw path string into a [`GaPath`] (the core parse).
    ///
    /// # Errors
    /// [`GaError::InvalidPath`] on a malformed address.
    pub fn parse_str(raw: &str) -> Result<Self, GaError> {
        let trimmed = raw.trim_end_matches('/');
        if trimmed == MOUNT || raw == MOUNT {
            return Ok(GaPath::Root);
        }
        let Some(after) = trimmed.strip_prefix(&format!("{MOUNT}/")) else {
            return Err(GaError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not under the /ga mount",
            });
        };

        let segments: Vec<&str> = after.split('/').filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            [] => Ok(GaPath::Root),
            [property] => Ok(GaPath::Property {
                property_id: (*property).to_string(),
            }),
            [property, sub] if *sub == REALTIME_SEGMENT => Ok(GaPath::Realtime {
                property_id: (*property).to_string(),
            }),
            [_property, other] => Err(GaError::InvalidPath {
                path: (*other).to_string(),
                reason: "a /ga property has only the core report and the `realtime` sub-surface",
            }),
            _ => Err(GaError::InvalidPath {
                path: raw.to_string(),
                reason: "a /ga path is /ga/<propertyId> or /ga/<propertyId>/realtime",
            }),
        }
    }

    /// The GA4 property id this address selects, if any. `None` for the virtual root.
    #[must_use]
    pub fn property_id(&self) -> Option<&str> {
        match self {
            GaPath::Property { property_id } | GaPath::Realtime { property_id } => {
                Some(property_id.as_str())
            }
            GaPath::Root => None,
        }
    }

    /// Whether this address selects the realtime report surface.
    #[must_use]
    pub const fn is_realtime(&self) -> bool {
        matches!(self, GaPath::Realtime { .. })
    }

    /// Whether this address selects a concrete property (core or realtime) — the queryable nodes.
    /// The virtual root is not queryable on its own.
    #[must_use]
    pub const fn is_property(&self) -> bool {
        matches!(self, GaPath::Property { .. } | GaPath::Realtime { .. })
    }
}
