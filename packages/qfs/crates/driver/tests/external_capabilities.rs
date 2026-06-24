//! F1 — external constructibility of [`Capabilities`].
//!
//! `Capabilities` is `#[non_exhaustive]` with public bool fields, so an out-of-crate
//! driver author **cannot** use a struct literal (E0639) and **cannot** use functional
//! struct update (`..Capabilities::default()`) to set a field either. An integration
//! test is a *separate crate*, so this file compiling at all is the proof that the
//! builder gives external authors a way to construct a `Capabilities` value. If the
//! builder regressed, this crate would fail to compile.

use qfs_driver::{Capabilities, Verb};

/// The chainable `none()` + `.select()/.insert()/...` builder constructs a value from
/// outside the defining crate, and `allows` reports exactly the enabled verbs.
#[test]
fn capabilities_builder_is_constructible_out_of_crate() {
    let caps = Capabilities::none().select().insert().update();

    assert!(caps.allows(Verb::Select));
    assert!(caps.allows(Verb::Insert));
    assert!(caps.allows(Verb::Update));
    assert!(!caps.allows(Verb::Remove));
    assert!(!caps.allows(Verb::Ls));

    // The declared verbs round-trip through the labels the structured error carries.
    assert_eq!(caps.supported_labels(), vec!["SELECT", "INSERT", "UPDATE"]);
}

/// The declarative `from_verbs` form is also reachable out-of-crate and agrees with the
/// chained form.
#[test]
fn capabilities_from_verbs_is_constructible_out_of_crate() {
    let blob = Capabilities::from_verbs(&[Verb::Ls, Verb::Cp, Verb::Mv, Verb::Rm]);

    assert!(blob.allows(Verb::Ls));
    assert!(blob.allows(Verb::Cp));
    assert!(blob.allows(Verb::Mv));
    assert!(blob.allows(Verb::Rm));
    assert!(!blob.allows(Verb::Select));

    // Equivalent to the chained builder.
    let chained = Capabilities::none().ls().cp().mv().rm();
    assert_eq!(blob, chained);
}
