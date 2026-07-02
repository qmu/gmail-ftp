//! ADR 0008 §5 — the **KeyGuardian composition root**: `qfs vault slots/enroll/revoke` and the
//! OS-keychain guardian (EPIC 20260702120000 / ticket 20260702120020).
//!
//! The vault's data-key is wrapped once per guardian into the LUKS-style `vault_key_slot` table
//! (the pure model: [`qfs_secrets::unlock_via_slots`]; the slot I/O: [`crate::secret_store`]).
//! This module owns the guardian **I/O** the pure layers must not touch:
//!
//! - **`keychain`** — a raw random KEK held by the platform secret service (macOS Keychain /
//!   Linux Secret Service via zbus; Windows Credential Manager). Once enrolled, every `qfs`
//!   invocation on this host unlocks the vault **without a passphrase or a prompt** — the answer
//!   to "a new tmux pane asks again". On a host with **no** secret service (headless servers,
//!   this EC2 box) [`keychain_kek`] reports the guardian unavailable (`None`) and enrolling fails
//!   with an actionable error — never a hang, never a panic.
//! - **`passphrase`** — lives in [`crate::connection`] (`resolve_store_passphrase`), enrolled as
//!   slot #1 by the first store open; `qfs vault rekey` re-wraps that slot only.
//!
//! ## Secret hygiene (RFD §10)
//! The KEK is 32 CSPRNG bytes minted here, hex-encoded into the keyring entry, and never logged,
//! printed, or placed on argv. `vault slots` renders slot *metadata* only (id, kind, created_at).

use qfs_cmd::VaultAction;

use crate::secret_store::GUARDIAN_KEYCHAIN;

/// The keyring coordinates of the keychain-held vault KEK. One entry per OS user — the same scope
/// as the Project DB the vault lives in.
const KEYRING_SERVICE: &str = "qfs";
const KEYRING_USER: &str = "vault-kek";

/// The injected vault launcher. Returns the process exit code (`0` ok, `1` on a structured,
/// secret-free error). Never panics.
#[must_use]
pub fn run_vault(action: &VaultAction) -> i32 {
    match run_inner(action) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("qfs: error: {e}");
            1
        }
    }
}

fn run_inner(action: &VaultAction) -> Result<String, String> {
    match action {
        VaultAction::Slots => list_slots(),
        VaultAction::Enroll { guardian } => match guardian.as_str() {
            GUARDIAN_KEYCHAIN => enroll_keychain(),
            other => Err(format!(
                "unknown guardian `{other}` — enrollable guardians: keychain (the passphrase \
                 slot is enrolled by the first store open; agent/KMS guardians are planned)"
            )),
        },
        VaultAction::Revoke { slot_id } => revoke_slot(*slot_id),
        VaultAction::Rekey => rekey_passphrase(),
    }
}

/// `qfs vault rekey` — re-wrap the store's data-key under a NEW passphrase (t79, moved here from
/// the retired `connection` namespace — the vault owns the store's key material). The OLD
/// passphrase is `QFS_PASSPHRASE` (the one that opened the store); the NEW one comes from stdin
/// (never argv). One re-wrap of the passphrase slot — existing secrets stay decryptable, the old
/// passphrase stops unlocking. A wrong old passphrase cannot reach here (the store would not open).
fn rekey_passphrase() -> Result<String, String> {
    use std::io::Read;
    let store = crate::connection::open_store()?;
    let old =
        std::env::var("QFS_PASSPHRASE").map_err(|_| "QFS_PASSPHRASE is not set".to_string())?;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("reading the new passphrase from stdin: {e}"))?;
    let new = buf.trim_end_matches(['\n', '\r']).to_string();
    if new.is_empty() {
        return Err(
            "no new passphrase on stdin — pipe it, e.g. `printf %s \"$NEWPASS\" | qfs vault rekey`"
                .into(),
        );
    }
    store
        .rewrap_passphrase(
            &qfs_secrets::Secret::from(old),
            &qfs_secrets::Secret::from(new),
        )
        .map_err(|e| format!("re-wrapping the data key: {e}"))?;
    crate::connection::emit_connection_audit("REKEY", "store");
    Ok(
        "re-wrapped the credential store under the new passphrase — set QFS_PASSPHRASE to the \
         new value for the next run"
            .into(),
    )
}

/// `qfs vault slots` — the slot metadata (id, guardian kind, created_at). Passphrase-free: slots
/// are public metadata (the wraps reveal nothing), so listing never unlocks or prompts.
fn list_slots() -> Result<String, String> {
    let conn = crate::connection::open_project_conn()?;
    let mut stmt = conn
        .prepare("SELECT slot_id, guardian_kind, created_at FROM vault_key_slot ORDER BY slot_id")
        .map_err(|e| format!("listing vault key slots: {e}"))?;
    let rows: Vec<(i64, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| format!("listing vault key slots: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("listing vault key slots: {e}"))?;
    if rows.is_empty() {
        return Ok(
            "no vault key slots yet — the store is created (with a passphrase slot) the first \
             time a credential is saved"
                .to_string(),
        );
    }
    let mut out = String::new();
    for (id, kind, created) in &rows {
        out.push_str(&format!("slot {id}\t{kind}\tcreated {created}\n"));
    }
    out.push_str(&format!("{} slot(s)", rows.len()));
    Ok(out)
}

/// `qfs vault enroll keychain` — mint a fresh random KEK, hand it to the platform secret service,
/// and wrap the (unlocked) store DEK under it as a new slot. From then on this host unlocks the
/// vault without a passphrase. Requires the store to be unlockable NOW (passphrase prompt/env) —
/// enrolling wraps the real DEK.
fn enroll_keychain() -> Result<String, String> {
    // Unlock first (the explicit, may-prompt path): enrolling needs the DEK.
    let store = crate::connection::open_store()?;
    // A second keychain slot would orphan the first's keyring entry (one entry per host) — refuse.
    let existing = store
        .list_slots()
        .map_err(|e| format!("listing vault key slots: {e}"))?;
    if existing
        .iter()
        .any(|(_, kind, _)| kind == GUARDIAN_KEYCHAIN)
    {
        return Err(
            "the OS keychain is already enrolled — revoke the existing keychain slot first \
             (qfs vault slots / qfs vault revoke <slot>)"
                .into(),
        );
    }

    let kek = qfs_secrets::generate_dek(); // 32 CSPRNG bytes serving as a raw KEK
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| keychain_unavailable("creating the keyring entry", &e))?;
    entry
        .set_password(&encode_kek(&kek))
        .map_err(|e| keychain_unavailable("storing the vault key in the OS keychain", &e))?;
    let slot = store
        .enroll_slot(GUARDIAN_KEYCHAIN, &kek, None)
        .map_err(|e| format!("enrolling the keychain slot: {e}"))?;
    Ok(format!(
        "enrolled the OS keychain as vault slot {slot} — qfs on this host now unlocks the \
         credential store without a passphrase (the passphrase slot remains as recovery)"
    ))
}

/// `qfs vault revoke <slot>` — delete one wrap (the last slot is refused by the store). Revoking
/// the keychain slot also best-effort deletes the keyring entry so no orphaned KEK lingers.
fn revoke_slot(slot_id: i64) -> Result<String, String> {
    let store = crate::connection::open_store()?;
    let slots = store
        .list_slots()
        .map_err(|e| format!("listing vault key slots: {e}"))?;
    let Some((_, kind, _)) = slots.iter().find(|(id, _, _)| *id == slot_id) else {
        return Err(format!(
            "no vault key slot {slot_id} — see `qfs vault slots`"
        ));
    };
    let kind = kind.clone();
    store
        .revoke_slot(slot_id)
        .map_err(|e| format!("revoking slot {slot_id}: {e}"))?;
    if kind == GUARDIAN_KEYCHAIN {
        // Best-effort: remove the now-useless keyring entry (its KEK wraps nothing anymore).
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
            let _ = entry.delete_credential();
        }
    }
    Ok(format!("revoked vault key slot {slot_id} ({kind})"))
}

/// The keychain-held KEK, if this host's secret service has one. `None` means the guardian is
/// unavailable here (nothing enrolled, or no secret service on a headless host) — callers skip
/// the keychain slot silently and fall through to the passphrase. Never errors, never blocks on
/// user interaction (the platform prompt, if any, is the OS's own unlock dialog).
#[must_use]
pub(crate) fn keychain_kek() -> Option<[u8; 32]> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    let encoded = entry.get_password().ok()?;
    decode_kek(&encoded)
}

/// An actionable "no secret service" error: what failed, why it usually fails (headless host),
/// and what still works. The keyring error text is operational metadata, never key material.
fn keychain_unavailable(op: &str, e: &keyring::Error) -> String {
    format!(
        "{op}: {e} — this host has no usable OS secret service (typical on a headless server: \
         no GNOME Keyring / KWallet on the D-Bus session). The passphrase guardian keeps \
         working; enroll the keychain on a desktop host instead"
    )
}

/// Hex-encode the KEK for the keyring entry (keyring stores strings).
fn encode_kek(kek: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in kek {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a hex KEK from the keyring entry; `None` on any malformation (treated as unavailable).
fn decode_kek(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The KEK hex codec round-trips, and malformed keyring contents read as "unavailable"
    /// (`None`) rather than a panic or a partial key.
    #[test]
    fn kek_hex_codec_round_trips_and_rejects_malformed() {
        let kek = qfs_secrets::generate_dek();
        let encoded = encode_kek(&kek);
        assert_eq!(encoded.len(), 64);
        assert_eq!(decode_kek(&encoded), Some(kek));
        // Whitespace tolerated (some keyrings append a newline); junk rejected.
        assert_eq!(decode_kek(&format!("{encoded}\n")), Some(kek));
        assert_eq!(decode_kek("not-hex"), None);
        assert_eq!(decode_kek(&encoded[..32]), None);
        assert_eq!(decode_kek(&format!("zz{}", &encoded[2..])), None);
    }
}
