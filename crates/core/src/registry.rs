//! The three **open registries** (RFD-0001 §3): paths/mounts, functions +
//! procedures, and codecs. These are the governance mechanism — "a new backend =
//! zero keywords" — so they must sit in the shared engine glue that both the CLI and
//! the server resolve through.
//!
//! Each registry is generic over a **trait object** (`Arc<dyn Driver>` /
//! `Arc<dyn Codec>` / an owned `ProcSig`), not over concrete types
//! (fidelity guard G2): a new driver (E4) implements the trait and calls `register`
//! — it touches zero core types. All three share the identical `new` / `register` /
//! `resolve` shape and use `BTreeMap` for deterministic iteration (test stability).
//! Empty at E0; the unit tests prove empty / round-trip / duplicate / absent.

use std::collections::BTreeMap;
use std::sync::Arc;

use cfs_codec::Codec;
use cfs_driver::{CfsError, Driver, ProcSig};

/// Registry of path mounts → drivers (RFD-0001 §3, "paths"). Keyed by mount string
/// (`/mail`, `/s3`, …).
#[derive(Default)]
pub struct MountRegistry {
    mounts: BTreeMap<String, Arc<dyn Driver>>,
}

impl MountRegistry {
    /// An empty mount registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a driver under its declared mount.
    ///
    /// # Errors
    /// [`CfsError::DuplicateRegistration`] if the mount is already taken.
    pub fn register(&mut self, driver: Arc<dyn Driver>) -> Result<(), CfsError> {
        let key = driver.mount().to_string();
        if self.mounts.contains_key(&key) {
            return Err(CfsError::DuplicateRegistration(key));
        }
        self.mounts.insert(key, driver);
        Ok(())
    }

    /// Resolve a mount to its driver.
    ///
    /// # Errors
    /// [`CfsError::UnknownMount`] if no driver is registered for the mount.
    pub fn resolve(&self, mount: &str) -> Result<Arc<dyn Driver>, CfsError> {
        self.mounts
            .get(mount)
            .cloned()
            .ok_or_else(|| CfsError::UnknownMount(mount.to_string()))
    }

    /// Number of registered mounts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }
}

/// Registry of functions + `CALL` procedures (RFD-0001 §3, "functions /
/// procedures"). One registry because both alias functions and procedures are
/// receiver-typed, registry-resolved, and keyword-free. Keyed by qualified name
/// (e.g. `mail.send`). Stores the [`ProcSig`] declaration (params, irreversible,
/// returns, requires_scopes — t13) only.
#[derive(Default)]
pub struct ProcRegistry {
    procs: BTreeMap<String, ProcSig>,
}

impl ProcRegistry {
    /// An empty procedure registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a procedure under a qualified name (e.g. `mail.send`).
    ///
    /// # Errors
    /// [`CfsError::DuplicateRegistration`] if the name is already taken.
    pub fn register(&mut self, qualified_name: &str, decl: ProcSig) -> Result<(), CfsError> {
        if self.procs.contains_key(qualified_name) {
            return Err(CfsError::DuplicateRegistration(qualified_name.to_string()));
        }
        self.procs.insert(qualified_name.to_string(), decl);
        Ok(())
    }

    /// Resolve a qualified procedure name to its declaration.
    ///
    /// # Errors
    /// [`CfsError::UnknownProcedure`] if the name is not registered.
    pub fn resolve(&self, qualified_name: &str) -> Result<&ProcSig, CfsError> {
        self.procs
            .get(qualified_name)
            .ok_or_else(|| CfsError::UnknownProcedure(qualified_name.to_string()))
    }

    /// Number of registered procedures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }
}

/// Registry of codecs (RFD-0001 §3, "codecs"). Keyed by format (`json`, `yaml`, …).
#[derive(Default)]
pub struct CodecRegistry {
    codecs: BTreeMap<String, Arc<dyn Codec>>,
}

impl CodecRegistry {
    /// An empty codec registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a codec under its declared format.
    ///
    /// # Errors
    /// [`CfsError::DuplicateRegistration`] if the format is already taken.
    pub fn register(&mut self, codec: Arc<dyn Codec>) -> Result<(), CfsError> {
        let key = codec.fmt().to_string();
        if self.codecs.contains_key(&key) {
            return Err(CfsError::DuplicateRegistration(key));
        }
        self.codecs.insert(key, codec);
        Ok(())
    }

    /// Resolve a format to its codec.
    ///
    /// # Errors
    /// [`CfsError::UnknownCodec`] if no codec is registered for the format.
    pub fn resolve(&self, fmt: &str) -> Result<Arc<dyn Codec>, CfsError> {
        self.codecs
            .get(fmt)
            .cloned()
            .ok_or_else(|| CfsError::UnknownCodec(fmt.to_string()))
    }

    /// Number of registered codecs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codecs.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codecs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfs_codec::{Codec, RowBatch};
    use cfs_driver::{Archetype, Capabilities, NodeDesc, Path, PushdownProfile, VersionSupport};
    use cfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
    use cfs_types::Schema;

    /// A no-I/O applier the fake driver hands back through the `applier()` seam.
    #[derive(Default)]
    struct NoopApplier;
    impl PlanApplier for NoopApplier {
        fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
            Ok(AppliedEffect::new(node.id, 0))
        }
    }

    struct FakeDriver {
        pushdown: PushdownProfile,
        applier: NoopApplier,
    }
    impl FakeDriver {
        fn new() -> Self {
            Self {
                pushdown: PushdownProfile::None,
                applier: NoopApplier,
            }
        }
    }
    impl Driver for FakeDriver {
        fn mount(&self) -> &str {
            "/fake"
        }
        fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
            let _ = NodeDesc::new(Archetype::BlobNamespace, Schema::empty());
            Err(CfsError::NotImplemented {
                feature: "describe",
            })
        }
        fn capabilities(&self, _p: &Path) -> Capabilities {
            Capabilities::default()
        }
        fn procedures(&self) -> &[ProcSig] {
            &[]
        }
        fn pushdown(&self) -> &PushdownProfile {
            &self.pushdown
        }
        fn version_support(&self, _p: &Path) -> VersionSupport {
            VersionSupport::None
        }
        fn applier(&self) -> &dyn PlanApplier {
            &self.applier
        }
    }

    struct FakeCodec;
    impl Codec for FakeCodec {
        fn fmt(&self) -> &str {
            "fake"
        }
        fn decode(&self, _b: &[u8]) -> Result<RowBatch, CfsError> {
            Err(CfsError::NotImplemented { feature: "decode" })
        }
        fn encode(&self, _r: &RowBatch) -> Result<Vec<u8>, CfsError> {
            Err(CfsError::NotImplemented { feature: "encode" })
        }
    }

    #[test]
    fn mount_registry_empty_then_roundtrip_then_duplicate_then_absent() {
        let mut reg = MountRegistry::new();
        assert!(reg.is_empty());
        assert!(matches!(
            reg.resolve("/fake"),
            Err(CfsError::UnknownMount(_))
        ));

        reg.register(Arc::new(FakeDriver::new())).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.resolve("/fake").unwrap().mount(), "/fake");

        let dup = reg.register(Arc::new(FakeDriver::new()));
        assert!(matches!(dup, Err(CfsError::DuplicateRegistration(_))));
    }

    #[test]
    fn proc_registry_empty_then_roundtrip_then_duplicate_then_absent() {
        let mut reg = ProcRegistry::new();
        assert!(reg.is_empty());
        assert!(matches!(
            reg.resolve("mail.send"),
            Err(CfsError::UnknownProcedure(_))
        ));

        let decl = ProcSig::new("send");
        reg.register("mail.send", decl.clone()).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.resolve("mail.send").unwrap().name, "send");

        let dup = reg.register("mail.send", decl);
        assert!(matches!(dup, Err(CfsError::DuplicateRegistration(_))));
    }

    #[test]
    fn codec_registry_empty_then_roundtrip_then_duplicate_then_absent() {
        let mut reg = CodecRegistry::new();
        assert!(reg.is_empty());
        assert!(matches!(
            reg.resolve("fake"),
            Err(CfsError::UnknownCodec(_))
        ));

        reg.register(Arc::new(FakeCodec)).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.resolve("fake").unwrap().fmt(), "fake");

        let dup = reg.register(Arc::new(FakeCodec));
        assert!(matches!(dup, Err(CfsError::DuplicateRegistration(_))));
    }
}
