//! t80 (roadmap **M5** — decision U / §4.5): the binary-side **per-recipient (end-to-end) DEK
//! store** — the DB + flow that makes a HIGH-SENSITIVITY connection's data-key recoverable only by an
//! authorized member, and **not by the server at rest**.
//!
//! ## Where this sits (and why here, not in a leaf)
//! The pure cryptographic recipient model lives in `qfs-oauth`
//! ([`qfs_oauth::wrap_dek_to_recipient`] / [`qfs_oauth::unwrap_dek_for_recipient`], over the vetted
//! `p256` ECDH the OAuth-AS already vendors). The pure attendance DECISION lives in `qfs-secrets`
//! ([`qfs_secrets::e2e_attendance_gate`]). This module is the **I/O that composes them** with the
//! Project DB — exactly where t43's [`crate::secret_store::SqliteSecrets`] and t81's
//! [`crate::shared_connection`] live, because the binary is the one place that owns a real DB
//! connection (decision F) and may depend on both `qfs-oauth` and `qfs-secrets`.
//!
//! ## The model (the opposite trust boundary to t43)
//! For a normal connection (t43) the server wraps the DEK under ONE passphrase-derived KEK it
//! re-derives, so it can execute a plan unattended. For a HIGH-SENSITIVITY connection, decision U
//! instead:
//!  - mints a FRESH per-connection DEK, seals the secret VALUE under it into the **`e2e_secret`**
//!    table (kept separate from the server-unwrappable `secret_store` on purpose), and
//!  - wraps that DEK **per recipient** — separately to each authorized member's PUBLIC key — into the
//!    **`e2e_recipient_wrap`** table, then DROPS the DEK.
//!
//! After that, the ONLY DEK material at rest is the per-recipient wraps. The server holds the
//! ciphertext but cannot recover the DEK by itself (the E2E property, §4.5 threat 3). A member
//! recovers it with THEIR private key ([`e2e_recover_dek`]); a NON-recipient cannot. Adding a
//! recipient re-wraps the DEK (an existing recipient, attended, authorizes it); removing a recipient
//! DROPS their wrap so they cannot unwrap NEW state (forward — a removed recipient who already saw a
//! secret is out of scope, like any E2E system).
//!
//! ## The explicit, audited trade-off (decision U / J)
//! Because no human key is in the loop on the autonomous commit path, an E2E connection is **refused
//! there** ([`e2e_bind_allowed`] runs [`qfs_secrets::e2e_attendance_gate`] with `attended = false`) —
//! it requires a human recipient unwrap. This is the t59-safety-mode-shaped gate: a high-sensitivity
//! connection cannot be used by an agent unattended.
//!
//! ## Secret hygiene (RFD §10)
//! The DEK and the recovered secret live only transiently in a redacting [`Secret`]; the private key
//! never touches the server. Every error here is value-free.

use qfs_oauth::{unwrap_dek_for_recipient, wrap_dek_to_recipient, RecipientKey};
use qfs_secrets::{
    e2e_attendance_gate, generate_dek, open, seal, E2eUseError, Secret, SecretError,
};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension};

/// The DEK / per-connection data-key width (ChaCha20-Poly1305 256-bit).
const DEK_LEN: usize = 32;
/// The ECDH ephemeral-scalar width (P-256).
const EPHEMERAL_LEN: usize = 32;
/// The AEAD nonce width for the per-recipient wrap (ChaCha20-Poly1305 96-bit).
const WRAP_NONCE_LEN: usize = 12;

/// An authorized recipient of a high-sensitivity connection: their identity `handle` (the
/// `/sys/users` primary email) plus their PUBLIC key (uncompressed SEC1 P-256 bytes). Carries no
/// private material — the public key is publishable metadata; the private key stays client-side.
#[derive(Debug, Clone)]
pub struct Recipient {
    /// The member's identity handle (the `/sys/users` primary_email).
    pub handle: String,
    /// The member's per-recipient PUBLIC key (uncompressed SEC1 P-256, 65 bytes).
    pub public_key_sec1: Vec<u8>,
}

/// Why a per-recipient (E2E) operation did not yield/seal a secret — structured and **secret-free**
/// (RFD §10): a connection name and a handle are metadata, never a credential. Mirrors the
/// secret-free taxonomies of [`SecretError`] / [`E2eUseError`].
#[derive(Debug)]
pub enum E2eError {
    /// The E2E attendance gate refused USE (the connection is high-sensitivity but the commit is
    /// unattended). The DEK was NEVER recovered — the fail-closed, "gated before unwrap" outcome.
    Attendance(E2eUseError),
    /// The caller's private key did not unwrap ANY of the connection's per-recipient wraps — they are
    /// not an authorized recipient. Fail-closed; the DEK is never recovered.
    NotARecipient,
    /// The connection has no sealed E2E value (it was never created E2E, or was removed).
    NotFound,
    /// A wrap/seal/open crypto step failed (a malformed public key, a corrupt wrap). Value-free.
    Crypto,
    /// An underlying DB failure (secret-free message).
    Backend(SecretError),
}

impl E2eError {
    /// A short, stable error code for structured surfaces / logs (secret-free).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            E2eError::Attendance(e) => e.code(),
            E2eError::NotARecipient => "e2e_not_a_recipient",
            E2eError::NotFound => "e2e_secret_not_found",
            E2eError::Crypto => "e2e_crypto_failed",
            E2eError::Backend(e) => e.code(),
        }
    }
}

impl std::fmt::Display for E2eError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            E2eError::Attendance(e) => write!(f, "{e}"),
            E2eError::NotARecipient => write!(
                f,
                "you are not an authorized recipient of this end-to-end connection"
            ),
            E2eError::NotFound => write!(f, "no end-to-end secret is stored for this connection"),
            E2eError::Crypto => write!(f, "an end-to-end wrap/unwrap operation failed"),
            E2eError::Backend(e) => write!(f, "{e}"),
        }
    }
}

/// Fill `buf` with fresh OS entropy. The binary owns the CSPRNG (the same `rand` edge t46/t48 use),
/// keeping the `qfs-oauth` wrap primitive itself off a `rand`/`getrandom` dependency.
fn fill_entropy(buf: &mut [u8]) {
    rand::rng().fill_bytes(buf);
}

// ---------------------------------------------------------------------------------------------
// Raw DB ops (selectors + opaque wrapped bytes; passphrase-free — these tables hold no
// server-decryptable key material).
// ---------------------------------------------------------------------------------------------

/// UPSERT a recipient's wrapped DEK for `(driver, connection)`. Adding a recipient (or re-wrapping
/// after a keypair rotation) is last-writer-wins per `(driver, connection, recipient)`.
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free).
pub fn db_add_recipient(
    conn: &Connection,
    driver: &str,
    connection: &str,
    recipient: &str,
    wrapped_dek: &[u8],
) -> Result<(), SecretError> {
    conn.execute(
        "INSERT INTO e2e_recipient_wrap (driver, connection, recipient, wrapped_dek) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(driver, connection, recipient) DO UPDATE SET \
             wrapped_dek = excluded.wrapped_dek, \
             created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        rusqlite::params![driver, connection, recipient, wrapped_dek],
    )
    .map_err(|e| SecretError::Backend(format!("adding E2E recipient: {e}")))?;
    Ok(())
}

/// Remove a recipient's wrapped DEK — they can no longer unwrap NEW state (forward security).
/// Idempotent: removing an absent recipient affects zero rows and is still `Ok`.
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free).
pub fn db_remove_recipient(
    conn: &Connection,
    driver: &str,
    connection: &str,
    recipient: &str,
) -> Result<(), SecretError> {
    conn.execute(
        "DELETE FROM e2e_recipient_wrap WHERE driver = ?1 AND connection = ?2 AND recipient = ?3",
        rusqlite::params![driver, connection, recipient],
    )
    .map_err(|e| SecretError::Backend(format!("removing E2E recipient: {e}")))?;
    Ok(())
}

/// Every `(recipient, wrapped_dek)` row for `(driver, connection)`, ordered by recipient. The wraps
/// are opaque — none is server-decryptable. Best-effort: a query failure yields an empty list.
#[must_use]
pub fn db_list_recipient_wraps(
    conn: &Connection,
    driver: &str,
    connection: &str,
) -> Vec<(String, Vec<u8>)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT recipient, wrapped_dek FROM e2e_recipient_wrap \
         WHERE driver = ?1 AND connection = ?2 ORDER BY recipient",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(rusqlite::params![driver, connection], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    }) else {
        return Vec::new();
    };
    rows.filter_map(Result::ok).collect()
}

/// The authorized recipient handles for `(driver, connection)` (metadata only).
#[must_use]
pub fn db_list_recipients(conn: &Connection, driver: &str, connection: &str) -> Vec<String> {
    db_list_recipient_wraps(conn, driver, connection)
        .into_iter()
        .map(|(handle, _)| handle)
        .collect()
}

/// Whether `(driver, connection)` is END-TO-END (high-sensitivity): true iff it has at least one
/// per-recipient wrap row. The presence of a row IS the E2E flag (no separate column needed).
/// Best-effort + passphrase-free; an unreadable DB reads as NOT E2E (the bind paths fail closed by
/// other means).
#[must_use]
pub fn db_is_e2e_connection(conn: &Connection, driver: &str, connection: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM e2e_recipient_wrap WHERE driver = ?1 AND connection = ?2 LIMIT 1",
        rusqlite::params![driver, connection],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// UPSERT the E2E sealed VALUE (nonce + ciphertext under the per-connection DEK) for
/// `(driver, connection)`. Kept in `e2e_secret`, separate from the server-unwrappable `secret_store`.
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free).
pub fn db_put_e2e_secret(
    conn: &Connection,
    driver: &str,
    connection: &str,
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<(), SecretError> {
    conn.execute(
        "INSERT INTO e2e_secret (driver, connection, nonce, ciphertext) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(driver, connection) DO UPDATE SET \
             nonce = excluded.nonce, \
             ciphertext = excluded.ciphertext, \
             created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        rusqlite::params![driver, connection, nonce, ciphertext],
    )
    .map_err(|e| SecretError::Backend(format!("storing E2E secret: {e}")))?;
    Ok(())
}

/// Read the E2E sealed value `(nonce, ciphertext)` for `(driver, connection)`, or `None`.
#[must_use]
pub fn db_get_e2e_secret(
    conn: &Connection,
    driver: &str,
    connection: &str,
) -> Option<(Vec<u8>, Vec<u8>)> {
    conn.query_row(
        "SELECT nonce, ciphertext FROM e2e_secret WHERE driver = ?1 AND connection = ?2",
        rusqlite::params![driver, connection],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------------------------
// High-level flow (composes the qfs-oauth recipient wrap + the qfs-secrets value seal).
// ---------------------------------------------------------------------------------------------

/// Create/replace a HIGH-SENSITIVITY (E2E) connection's secret: mint a fresh per-connection DEK, seal
/// `value` under it into `e2e_secret`, wrap that DEK to EACH recipient's public key into
/// `e2e_recipient_wrap`, then drop the DEK. After this, the server holds the ciphertext + the
/// per-recipient wraps but cannot recover the DEK by itself — only an authorized recipient can.
///
/// `recipients` must be non-empty (an E2E connection with no recipient is unrecoverable by design and
/// rejected). Each public key is the member's registered `/sys/users.public_key`.
///
/// # Errors
/// [`E2eError::NotARecipient`] is never returned here; [`E2eError::Crypto`] if a recipient public key
/// is malformed; [`E2eError::Backend`] on a DB failure. All value-free.
pub fn e2e_put(
    conn: &Connection,
    driver: &str,
    connection: &str,
    recipients: &[Recipient],
    value: &Secret,
) -> Result<(), E2eError> {
    if recipients.is_empty() {
        // An E2E secret with no recipient could never be unwrapped — refuse rather than orphan it.
        return Err(E2eError::NotARecipient);
    }

    // Mint a fresh per-connection DEK and seal the value under it (separate from secret_store).
    let dek = generate_dek();
    let (nonce, ciphertext) = seal(&dek, value.expose()).map_err(|_| E2eError::Crypto)?;
    db_put_e2e_secret(conn, driver, connection, &nonce, &ciphertext).map_err(E2eError::Backend)?;

    // Wrap the DEK to each recipient's public key (fresh ephemeral keypair + nonce per wrap).
    let dek_secret = Secret::new(dek.to_vec());
    for r in recipients {
        wrap_for_recipient(
            conn,
            driver,
            connection,
            &r.handle,
            &r.public_key_sec1,
            &dek_secret,
        )?;
    }
    // `dek` / `dek_secret` drop (zeroized) here — the server retains only the per-recipient wraps.
    Ok(())
}

/// Wrap `dek_secret` to one recipient's public key and persist the row (a private helper shared by
/// [`e2e_put`] and [`e2e_add_recipient`]). Draws a fresh ephemeral keypair + nonce from the CSPRNG.
fn wrap_for_recipient(
    conn: &Connection,
    driver: &str,
    connection: &str,
    handle: &str,
    public_key_sec1: &[u8],
    dek_secret: &Secret,
) -> Result<(), E2eError> {
    let mut ephemeral = [0u8; EPHEMERAL_LEN];
    let mut wrap_nonce = [0u8; WRAP_NONCE_LEN];
    fill_entropy(&mut ephemeral);
    fill_entropy(&mut wrap_nonce);
    let wrapped = wrap_dek_to_recipient(public_key_sec1, dek_secret, &ephemeral, &wrap_nonce)
        .map_err(|_| E2eError::Crypto)?;
    db_add_recipient(conn, driver, connection, handle, &wrapped).map_err(E2eError::Backend)
}

/// Recover the per-connection DEK with a recipient's PRIVATE key: try the key against every stored
/// wrap and return the DEK from the one that opens (proving the caller is an authorized recipient). A
/// NON-recipient opens none and gets [`E2eError::NotARecipient`] — fail closed.
///
/// This is the human-in-the-loop unwrap: it needs the recipient's private key, which the server never
/// holds. The recovered DEK lives only inside the returned [`Secret`].
///
/// # Errors
/// [`E2eError::NotARecipient`] if the key unwraps none of the connection's wraps.
pub fn e2e_recover_dek(
    conn: &Connection,
    driver: &str,
    connection: &str,
    recipient_key: &RecipientKey,
) -> Result<Secret, E2eError> {
    for (_handle, wrapped) in db_list_recipient_wraps(conn, driver, connection) {
        if let Ok(dek) = unwrap_dek_for_recipient(recipient_key, &wrapped) {
            return Ok(dek);
        }
    }
    Err(E2eError::NotARecipient)
}

/// **Open** the E2E secret value for `(driver, connection)` as a recipient, gating on attendance
/// FIRST. The pure [`e2e_attendance_gate`] runs before any unwrap: an E2E credential used
/// `attended == false` (an autonomous agent / server-fire) is refused with [`E2eError::Attendance`]
/// and the DEK is NEVER recovered. With a human in the loop (`attended == true`), the recipient's
/// private key recovers the DEK and opens the value into a redacting [`Secret`].
///
/// # Errors
/// [`E2eError::Attendance`] when refused unattended; [`E2eError::NotARecipient`] if `recipient_key`
/// is not authorized; [`E2eError::NotFound`] if no sealed value exists; [`E2eError::Crypto`] on a
/// corrupt value. All value-free.
pub fn e2e_open(
    conn: &Connection,
    driver: &str,
    connection: &str,
    recipient_key: &RecipientKey,
    attended: bool,
) -> Result<Secret, E2eError> {
    // GATE BEFORE UNWRAP: a high-sensitivity credential used unattended is refused here, before the
    // per-recipient DEK is ever recovered (the t59-safety-mode-shaped, audited trade-off).
    e2e_attendance_gate(true, connection, attended).map_err(E2eError::Attendance)?;

    let dek_secret = e2e_recover_dek(conn, driver, connection, recipient_key)?;
    let dek: [u8; DEK_LEN] = dek_secret
        .expose()
        .try_into()
        .map_err(|_| E2eError::Crypto)?;
    let (nonce, ciphertext) =
        db_get_e2e_secret(conn, driver, connection).ok_or(E2eError::NotFound)?;
    let plaintext = open(&dek, &nonce, &ciphertext).map_err(|_| E2eError::Crypto)?;
    Ok(Secret::new(plaintext))
}

/// **Add a recipient** to an existing E2E connection: an EXISTING recipient (`authorizer`, attended,
/// holding their private key) recovers the DEK and re-wraps it to the `new` recipient's public key.
/// The server cannot do this alone — recovering the DEK requires an authorized private key, which is
/// the whole point. Removing is the dual: [`db_remove_recipient`] drops a row.
///
/// # Errors
/// [`E2eError::NotARecipient`] if `authorizer` is not already a recipient; [`E2eError::Crypto`] if the
/// new public key is malformed; [`E2eError::Backend`] on a DB failure.
pub fn e2e_add_recipient(
    conn: &Connection,
    driver: &str,
    connection: &str,
    authorizer: &RecipientKey,
    new: &Recipient,
) -> Result<(), E2eError> {
    let dek_secret = e2e_recover_dek(conn, driver, connection, authorizer)?;
    wrap_for_recipient(
        conn,
        driver,
        connection,
        &new.handle,
        &new.public_key_sec1,
        &dek_secret,
    )
}

// ---------------------------------------------------------------------------------------------
// Production wiring: the live commit registry consults the gate so it is never inert.
// ---------------------------------------------------------------------------------------------

/// The commit-time gate the live credential registry consults before binding a connection on the
/// AUTONOMOUS path (t80). Returns `true` to allow the bind, `false` to refuse it (the driver is then
/// left UNREGISTERED — fail closed, exactly like t54's cloud consent gate and t81's shared-use gate).
///
/// - A **non-E2E** connection (no `e2e_recipient_wrap` row) is never gated here ⇒ `true`. Every
///   existing managed-tier flow is unchanged.
/// - An **E2E** connection is refused on this path: the autonomous commit registry has NO human key
///   in the loop, so [`e2e_attendance_gate`] (with `attended = false`) denies it. Such a connection
///   requires a human recipient to unwrap it ([`e2e_open`] with `attended = true`).
///
/// Best-effort + passphrase-free: it reads only the E2E flag (a metadata row), never a token, BEFORE
/// any decrypt. The refusal reason is logged secret-free so the operator sees WHY.
#[must_use]
pub fn e2e_bind_allowed(driver: &str, connection: &str) -> bool {
    let Some(proj) = crate::store::open_project_db().ok().flatten() else {
        // No project DB ⇒ no E2E connections recorded ⇒ nothing is high-sensitivity ⇒ ungated.
        return true;
    };
    let conn = proj.into_db().into_connection();
    let is_e2e = db_is_e2e_connection(&conn, driver, connection);
    // The autonomous commit registry is unattended by construction (no human key in the loop).
    match e2e_attendance_gate(is_e2e, connection, /* attended = */ false) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                target: "qfs::e2e",
                "end-to-end connection '{driver}/{connection}' not bound: {} ({})",
                e,
                e.code()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_store::{MemorySource, ProjectDb};

    const PLANTED: &str = "ghp_E2E_SECRET_LEAK_CANARY_77";

    fn migrated_conn() -> Connection {
        ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection()
    }

    /// Generate a recipient keypair from fixed-ish entropy (hermetic, no network/credentials).
    fn keypair(seed: u8) -> RecipientKey {
        let mut e = [seed; 32];
        // Perturb so the scalar is non-trivial and distinct per seed.
        e[0] = seed.wrapping_add(1);
        e[31] = seed.wrapping_mul(3).wrapping_add(7);
        RecipientKey::generate(&e).unwrap()
    }

    fn recipient(handle: &str, key: &RecipientKey) -> Recipient {
        Recipient {
            handle: handle.to_string(),
            public_key_sec1: key.public_key_sec1(),
        }
    }

    /// The headline E2E round-trip: a value put under recipient A's key is opened by A (attended), and
    /// recipient B (NOT a recipient) cannot open it — the secret never surfaces to a non-recipient.
    #[test]
    fn a_recipient_opens_the_secret_and_a_non_recipient_cannot() {
        let conn = migrated_conn();
        let alice = keypair(1);
        let bob = keypair(2);

        e2e_put(
            &conn,
            "vault",
            "prod",
            &[recipient("alice@team.io", &alice)],
            &Secret::from(PLANTED),
        )
        .unwrap();

        // Alice (a recipient), attended, recovers the exact secret.
        let got = e2e_open(&conn, "vault", "prod", &alice, true).unwrap();
        assert_eq!(got.expose_str(), Some(PLANTED));

        // Bob (NOT a recipient) is refused — the DEK is never recovered for him.
        let err = e2e_open(&conn, "vault", "prod", &bob, true).unwrap_err();
        assert_eq!(err.code(), "e2e_not_a_recipient");
        assert!(!format!("{err:?} {err}").contains(PLANTED));
    }

    /// An E2E credential used UNATTENDED (an autonomous agent) is refused BEFORE any unwrap — and the
    /// live bind gate refuses it too. With a human in the loop it succeeds.
    #[test]
    fn an_unattended_open_is_refused_pending_human_unwrap() {
        let conn = migrated_conn();
        let alice = keypair(3);
        e2e_put(
            &conn,
            "vault",
            "prod",
            &[recipient("alice@team.io", &alice)],
            &Secret::from(PLANTED),
        )
        .unwrap();

        // Unattended (no human key in the loop) ⇒ refused by the attendance gate, no DEK recovered.
        let err = e2e_open(&conn, "vault", "prod", &alice, /* attended = */ false).unwrap_err();
        assert_eq!(err.code(), "e2e_connection_unattended");
        assert!(!format!("{err:?} {err}").contains(PLANTED));

        // The connection is flagged E2E (presence of a wrap row).
        assert!(db_is_e2e_connection(&conn, "vault", "prod"));

        // Attended ⇒ the same recipient opens it.
        assert_eq!(
            e2e_open(&conn, "vault", "prod", &alice, true)
                .unwrap()
                .expose_str(),
            Some(PLANTED)
        );
    }

    /// Adding a recipient (authorized by an existing one) lets the NEW member open the secret;
    /// removing a recipient drops their wrap so they can no longer recover the DEK (forward security).
    #[test]
    fn adding_a_recipient_grants_access_and_removing_one_revokes_future_access() {
        let conn = migrated_conn();
        let alice = keypair(4);
        let bob = keypair(5);

        e2e_put(
            &conn,
            "vault",
            "prod",
            &[recipient("alice@team.io", &alice)],
            &Secret::from(PLANTED),
        )
        .unwrap();

        // Bob cannot open it yet (not a recipient).
        assert_eq!(
            e2e_open(&conn, "vault", "prod", &bob, true)
                .unwrap_err()
                .code(),
            "e2e_not_a_recipient"
        );

        // Alice (an existing recipient) authorizes adding Bob.
        e2e_add_recipient(
            &conn,
            "vault",
            "prod",
            &alice,
            &recipient("bob@team.io", &bob),
        )
        .unwrap();
        assert_eq!(
            e2e_open(&conn, "vault", "prod", &bob, true)
                .unwrap()
                .expose_str(),
            Some(PLANTED),
            "an added recipient can now open the secret"
        );
        assert_eq!(db_list_recipients(&conn, "vault", "prod").len(), 2);

        // Remove Bob — his wrap is dropped, so he can no longer recover the DEK from stored state.
        db_remove_recipient(&conn, "vault", "prod", "bob@team.io").unwrap();
        assert_eq!(
            e2e_open(&conn, "vault", "prod", &bob, true)
                .unwrap_err()
                .code(),
            "e2e_not_a_recipient",
            "a removed recipient can no longer unwrap"
        );
        // Alice is unaffected.
        assert_eq!(
            e2e_open(&conn, "vault", "prod", &alice, true)
                .unwrap()
                .expose_str(),
            Some(PLANTED)
        );
    }

    /// The server-stored state alone (the sealed value + the per-recipient wraps) does NOT contain the
    /// DEK or the plaintext: there is NO passphrase-wrapped DEK at rest, so the server cannot decrypt.
    #[test]
    fn the_server_stored_ciphertext_alone_does_not_yield_the_dek_or_plaintext() {
        let conn = migrated_conn();
        let alice = keypair(6);
        e2e_put(
            &conn,
            "vault",
            "prod",
            &[recipient("alice@team.io", &alice)],
            &Secret::from(PLANTED),
        )
        .unwrap();

        // The sealed value column never contains the plaintext.
        let (_n, ct) = db_get_e2e_secret(&conn, "vault", "prod").unwrap();
        assert!(
            !ct.windows(PLANTED.len()).any(|w| w == PLANTED.as_bytes()),
            "plaintext leaked into the E2E ciphertext"
        );

        // There is NO server-unwrappable DEK: `secret_store` / `secret_meta` carry nothing for this
        // connection, and the ONLY DEK material at rest is the per-recipient wraps (not decryptable
        // without a private key the server does not hold).
        let secret_store_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM secret_store WHERE driver = 'vault' AND connection = 'prod'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            secret_store_rows, 0,
            "an E2E value must NOT live in the server-unwrappable secret_store"
        );
        // The per-recipient wrap exists but does not contain the plaintext either.
        let wraps = db_list_recipient_wraps(&conn, "vault", "prod");
        assert_eq!(wraps.len(), 1);
        assert!(!wraps[0]
            .1
            .windows(PLANTED.len())
            .any(|w| w == PLANTED.as_bytes()));
    }

    /// An E2E connection with no recipients is rejected (it could never be unwrapped).
    #[test]
    fn an_e2e_connection_requires_at_least_one_recipient() {
        let conn = migrated_conn();
        let err = e2e_put(&conn, "vault", "prod", &[], &Secret::from(PLANTED)).unwrap_err();
        assert_eq!(err.code(), "e2e_not_a_recipient");
        assert!(!db_is_e2e_connection(&conn, "vault", "prod"));
    }
}
