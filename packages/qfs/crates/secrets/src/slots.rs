//! [`slots`](self) — the pure **KeyGuardian vault-key-slot** model (ADR 0008 §5, EPIC
//! 20260702120000 / ticket 20260702120020).
//!
//! LUKS-style key slots generalize the single passphrase-wrapped DEK: the SAME 32-byte data-key
//! is [`crate::wrap_dek`]ped **once per guardian** — the passphrase (argon2id-derived KEK, slot
//! kind `passphrase`), the OS keychain (a random KEK held by the platform secret service, kind
//! `keychain`), and later an agent or a managed KMS — and the wraps sit side by side. Unlocking
//! tries each slot whose guardian can produce a KEK; **any one** slot opens the store. Enrolling
//! a slot wraps the DEK under one more KEK without re-sealing a single value; revoking deletes a
//! wrap. t80's per-recipient E2E wrap (`e2e_recipient_wrap`) is the same mechanism for members —
//! this module is its guardian-shaped sibling.
//!
//! Like [`crate::envelope`] this is **pure** (no DB, no keyring, no prompt): the guardian I/O —
//! reading `QFS_PASSPHRASE`, prompting a TTY, talking to the platform secret service — lives in
//! the terminal binary, which passes a `kek_of` resolver in. That split keeps the slot logic
//! hermetically testable and the crate wasm-buildable.
//!
//! ## Secret hygiene
//! The unlock failure is the value-free [`EnvelopeError`]: whether NO guardian could produce a
//! KEK, or every produced KEK failed to unwrap (wrong passphrase / tampered wrap), the caller
//! learns only "locked" — never **which** slot failed (RFD §10).

use crate::envelope::{unwrap_dek, EnvelopeError};

/// One vault-key slot row: the SAME store DEK wrapped under this guardian's KEK. Selectors +
/// wrapped material only — a wrap reveals nothing without the guardian's KEK, and the KDF salt is
/// public metadata (present only for KDF-derived guardians such as the passphrase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotWrap {
    /// The slot's DB id (stable across the store's life; what `vault revoke <slot>` names).
    pub slot_id: i64,
    /// The guardian kind that can produce this slot's KEK: `passphrase` | `keychain` (agent / KMS
    /// kinds arrive with their guardians). A plain string so new kinds are additive.
    pub guardian_kind: String,
    /// The store DEK, AEAD-wrapped under this slot's KEK ([`crate::wrap_dek`] format).
    pub wrapped_dek: Vec<u8>,
    /// The per-slot KDF salt for a derived KEK (the passphrase guardian); `None` for guardians
    /// holding a raw random KEK (keychain).
    pub kdf_salt: Option<Vec<u8>>,
}

/// Unlock the vault through the slot set: for each slot (in the given order) ask `kek_of` for the
/// guardian's KEK — `None` means "this guardian is unavailable here" (no passphrase supplied, no
/// secret service on the host) and the slot is skipped silently — and return the DEK from the
/// **first** wrap that opens. Slot order is the caller's preference order (e.g. keychain before
/// passphrase, so an enrolled keychain avoids a prompt).
///
/// # Errors
/// [`EnvelopeError`] when no slot opens — whether no guardian produced a KEK or every KEK failed
/// authentication. Deliberately indistinguishable: the error never names a slot.
pub fn unlock_via_slots<F>(slots: &[SlotWrap], mut kek_of: F) -> Result<[u8; 32], EnvelopeError>
where
    F: FnMut(&SlotWrap) -> Option<[u8; 32]>,
{
    for slot in slots {
        if let Some(kek) = kek_of(slot) {
            if let Ok(dek) = unwrap_dek(&kek, &slot.wrapped_dek) {
                return Ok(dek);
            }
        }
    }
    Err(EnvelopeError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{derive_kek, generate_dek, generate_salt, wrap_dek};

    fn passphrase_slot(id: i64, pass: &[u8], dek: &[u8; 32]) -> (SlotWrap, [u8; 32]) {
        let salt = generate_salt();
        let kek = derive_kek(pass, &salt).unwrap();
        (
            SlotWrap {
                slot_id: id,
                guardian_kind: "passphrase".into(),
                wrapped_dek: wrap_dek(&kek, dek).unwrap(),
                kdf_salt: Some(salt.to_vec()),
            },
            kek,
        )
    }

    fn raw_kek_slot(id: i64, kind: &str, dek: &[u8; 32]) -> (SlotWrap, [u8; 32]) {
        let kek = generate_dek(); // any random 32 bytes serve as a raw KEK
        (
            SlotWrap {
                slot_id: id,
                guardian_kind: kind.into(),
                wrapped_dek: wrap_dek(&kek, dek).unwrap(),
                kdf_salt: None,
            },
            kek,
        )
    }

    /// The point of the slot set: EITHER guardian alone recovers the same DEK — the keychain slot
    /// with the passphrase unavailable, and the passphrase slot with the keychain unavailable.
    #[test]
    fn either_slot_alone_unlocks_the_same_dek() {
        let dek = generate_dek();
        let (pass_slot, pass_kek) = passphrase_slot(1, b"correct horse", &dek);
        let (chain_slot, chain_kek) = raw_kek_slot(2, "keychain", &dek);
        let slots = [chain_slot.clone(), pass_slot.clone()];

        // Keychain available, passphrase not (no prompt, no env): the keychain slot opens.
        let got = unlock_via_slots(&slots, |s| {
            (s.guardian_kind == "keychain").then_some(chain_kek)
        })
        .unwrap();
        assert_eq!(got, dek);

        // Passphrase available, keychain absent (headless host): the passphrase slot opens.
        let got = unlock_via_slots(&slots, |s| {
            (s.guardian_kind == "passphrase").then_some(pass_kek)
        })
        .unwrap();
        assert_eq!(got, dek);
    }

    /// An unavailable guardian is skipped silently — `kek_of` returning `None` must not abort the
    /// scan before a later slot gets its chance (the headless-keychain case).
    #[test]
    fn unavailable_guardians_are_skipped_not_fatal() {
        let dek = generate_dek();
        let (pass_slot, pass_kek) = passphrase_slot(1, b"pw", &dek);
        let (chain_slot, _lost) = raw_kek_slot(2, "keychain", &dek);
        // Keychain first in preference order but unavailable; passphrase still opens.
        let slots = [chain_slot, pass_slot];
        let got = unlock_via_slots(&slots, |s| match s.guardian_kind.as_str() {
            "passphrase" => Some(pass_kek),
            _ => None,
        })
        .unwrap();
        assert_eq!(got, dek);
    }

    /// A wrong passphrase on a multi-slot store yields the single value-free error — the caller
    /// cannot tell WHICH slot failed, or whether the other guardian was even consulted.
    #[test]
    fn wrong_keks_yield_one_indistinguishable_error() {
        let dek = generate_dek();
        let (pass_slot, _right) = passphrase_slot(1, b"right", &dek);
        let (chain_slot, _kek) = raw_kek_slot(2, "keychain", &dek);
        let slots = [chain_slot, pass_slot.clone()];

        let wrong = derive_kek(b"wrong", pass_slot.kdf_salt.as_deref().unwrap()).unwrap();
        let err = unlock_via_slots(&slots, |s| {
            (s.guardian_kind == "passphrase").then_some(wrong)
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "envelope crypto operation failed (wrong key or corrupt data)"
        );
        // No guardian available at all: the SAME error (indistinguishable).
        let err2 = unlock_via_slots(&slots, |_| None).unwrap_err();
        assert_eq!(err.to_string(), err2.to_string());
    }

    /// An empty slot set is locked (a fresh store has nothing to unlock — init is a different path).
    #[test]
    fn empty_slot_set_is_locked() {
        assert!(unlock_via_slots(&[], |_| Some([0u8; 32])).is_err());
    }
}
