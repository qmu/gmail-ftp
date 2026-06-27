//! [`CfsError`] — the one structured, machine-readable error enum shared
//! workspace-wide (RFD-0001 §5: errors must be parseable by an AI, not prose).
//!
//! ## Why this lives in `qfs-driver` (decision D1)
//! The design (design-v1 §2.3) nominally placed `CfsError` in `qfs-core`, but the
//! `Driver` and `Codec` trait signatures both return `Result<_, CfsError>`, and the
//! acyclic dependency spine (model-v1 §4) requires `qfs-core → qfs-driver`. Placing
//! the error in `qfs-core` would force a back-edge `qfs-driver → qfs-core` — a cycle
//! the model explicitly forbids. The error therefore lives in the lowest crate the
//! trait signatures need (`qfs-driver`), `qfs-codec` depends on `qfs-driver` for it,
//! and `qfs-core` **re-exports** it so the rest of the workspace still sees a single
//! `qfs_core::CfsError`. This preserves "one error enum, shared workspace-wide"
//! while keeping the spine strictly acyclic.

/// The single structured error type for qfs.
///
/// Variants are `#[non_exhaustive]`: epics add arms as features land. Every arm is
/// machine-readable via [`CfsError::code`]; rendering is the caller's concern (the
/// `qfs-cmd` boundary chooses human vs. JSON envelope).
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
    /// `qfs-parser::ParseError` maps into this arm).
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

    /// A driver tried to register under a mount that shadows a reserved **scope realm**
    /// (decision P / RFD §1.3) — e.g. `/members`, `/projects`, `/hosts`, `/directories`,
    /// `/me`. Root is a closed set of realms, and the `(scope, service)` split a path
    /// resolves through is only decidable if **no** driver mount is named after a realm,
    /// so registration is rejected up front. The sole driver-backed realm `/sys` is
    /// exempt (its realm and mount coincide). Carries the offending mount and the realm
    /// it shadowed so an AI-facing caller can recover without prose-parsing.
    #[error("mount {mount:?} shadows reserved realm /{realm}")]
    ReservedRealmMount {
        /// The mount the driver tried to register under (e.g. `/members`).
        mount: String,
        /// The reserved realm name the mount shadowed (e.g. `members`).
        realm: &'static str,
    },

    /// A server boot / hot-reconfigure failure (E7, `qfs serve`). Carries a stable,
    /// secret-free machine code and a line-located message produced by `qfs-server`;
    /// `qfs-driver` cannot name `qfs_server::ServerError` (that crate is far above it in
    /// the spine), so the structured fields are flattened into owned data here. The server
    /// runtime maps its own `ServerError` into this arm at the `serve` boundary.
    #[error("server config error: {message}")]
    Server {
        /// The originating server error's stable code (e.g. `config_parse`).
        server_code: String,
        /// The line-located, secret-free message.
        message: String,
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
            Self::ReservedRealmMount { .. } => "reserved_realm_mount",
            // The granular server error code lives in `server_code`; the workspace-level
            // code is the stable family label.
            Self::Server { .. } => "server_config",
        }
    }
}
