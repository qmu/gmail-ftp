//! [`CfsError`] — the one structured, machine-readable error enum shared
//! workspace-wide (RFD-0001 §5: errors must be parseable by an AI, not prose).
//!
//! ## Why this lives in `cfs-driver` (decision D1)
//! The design (design-v1 §2.3) nominally placed `CfsError` in `cfs-core`, but the
//! `Driver` and `Codec` trait signatures both return `Result<_, CfsError>`, and the
//! acyclic dependency spine (model-v1 §4) requires `cfs-core → cfs-driver`. Placing
//! the error in `cfs-core` would force a back-edge `cfs-driver → cfs-core` — a cycle
//! the model explicitly forbids. The error therefore lives in the lowest crate the
//! trait signatures need (`cfs-driver`), `cfs-codec` depends on `cfs-driver` for it,
//! and `cfs-core` **re-exports** it so the rest of the workspace still sees a single
//! `cfs_core::CfsError`. This preserves "one error enum, shared workspace-wide"
//! while keeping the spine strictly acyclic.

/// The single structured error type for cfs.
///
/// Variants are `#[non_exhaustive]`: epics add arms as features land. Every arm is
/// machine-readable via [`CfsError::code`]; rendering is the caller's concern (the
/// `cfs-cmd` boundary chooses human vs. JSON envelope).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CfsError {
    /// A feature that is reserved but not yet implemented at this epic.
    #[error("not yet implemented: {feature}")]
    NotImplemented {
        /// A stable, machine-facing identifier for the missing feature.
        feature: &'static str,
    },

    /// A mount path was resolved against the `MountRegistry` but no driver is
    /// registered for it.
    #[error("unknown mount: {0}")]
    UnknownMount(String),

    /// A procedure / function name was resolved against the `ProcRegistry` but is
    /// not registered.
    #[error("unknown procedure: {0}")]
    UnknownProcedure(String),

    /// A codec format was resolved against the `CodecRegistry` but is not
    /// registered.
    #[error("unknown codec format: {0}")]
    UnknownCodec(String),

    /// An attempt to register an item under a key that is already taken.
    #[error("duplicate registration for key: {0}")]
    DuplicateRegistration(String),

    /// A parse failure. Populated with span / expected-set detail in E1 (the owned
    /// `cfs-parser::ParseError` maps into this arm).
    #[error("parse error")]
    Parse,

    /// A codec failed to decode bytes for its format (RFD §4/§5). Structured so an AI
    /// gets a typed parse error, not prose: names the `fmt` and a machine-facing
    /// `detail` (where available, a line/offset hint is folded into `detail`).
    #[error("decode error ({fmt}): {detail}")]
    Decode {
        /// The codec format that failed to decode (e.g. `json`, `csv`).
        fmt: &'static str,
        /// A machine-facing reason (the underlying parser message / location).
        detail: String,
    },

    /// A codec failed to encode a [`crate::CfsError`]-bearing row batch back to bytes
    /// for its format (RFD §4). Structured for AI recovery: names the `fmt` and reason.
    #[error("encode error ({fmt}): {detail}")]
    Encode {
        /// The codec format that failed to encode (e.g. `toml`, `csv`).
        fmt: &'static str,
        /// A machine-facing reason the rows could not be encoded.
        detail: String,
    },

    /// A virtual path was malformed at the driver boundary (empty, or not absolute).
    /// Carries the offending text and a stable machine-facing reason (RFD §5).
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath {
        /// The offending path text.
        path: String,
        /// A stable, machine-facing reason the path was rejected.
        reason: &'static str,
    },

    /// A verb was planned against a node whose driver does not declare it — the
    /// **parse/resolve-time capability gate** (RFD §5). Structured for AI consumption:
    /// it names the path, the rejected verb, and the verbs the node *does* support so
    /// the caller (or the AI) can recover without prose-parsing.
    #[error("unsupported verb {verb} at {path:?}; supported: [{}]", supported.join(", "))]
    UnsupportedVerb {
        /// The path the verb was planned against.
        path: String,
        /// The rejected verb's stable label (e.g. `UPDATE`).
        verb: &'static str,
        /// The verbs the node does support, as stable labels (for AI recovery).
        supported: Vec<&'static str>,
    },
}

impl CfsError {
    /// A stable, machine-readable code for this error. AI-facing callers branch on
    /// this rather than on the human message (RFD §5).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotImplemented { .. } => "not_implemented",
            Self::UnknownMount(_) => "unknown_mount",
            Self::UnknownProcedure(_) => "unknown_procedure",
            Self::UnknownCodec(_) => "unknown_codec",
            Self::DuplicateRegistration(_) => "duplicate_registration",
            Self::Parse => "parse_error",
            Self::Decode { .. } => "decode_error",
            Self::Encode { .. } => "encode_error",
            Self::InvalidPath { .. } => "invalid_path",
            Self::UnsupportedVerb { .. } => "unsupported_verb",
        }
    }
}
