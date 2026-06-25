//! [`LocalStore`] — the native, encrypted-at-rest credential backend (RFD-0001 §10).
//!
//! One encrypted blob at `~/.config/qfs/credentials` (XDG), mode `0600`, holding the whole
//! `(driver, account) -> secret` map. AEAD is ChaCha20-Poly1305; the key comes from a
//! caller-supplied 32-byte key (the OS-keyring path in production) or is derived from a
//! passphrase with argon2id ([`LocalStore::from_passphrase`]). Writes are atomic
//! (temp-file + `rename`) so a crash mid-write never corrupts the prior blob.
//!
//! Compiled only on non-wasm targets — there is no filesystem on Cloudflare Workers, so
//! the `0600`/AEAD-file path is *compiled out*, not skipped at runtime; the wasm build
//! uses [`crate::WorkerStore`] instead (see `worker.rs`).
//!
//! ## Threat model (documented per the ticket)
//! At-rest confidentiality only. The blob is unreadable without the key, and `0600`
//! keeps group/other off it. A host already compromised *with* the decryption key (or the
//! live process memory) is explicitly **out of scope** — no software store defends that.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::Rng;
use time::OffsetDateTime;

use crate::key::{AccountId, AccountRecord, CredentialKey, DriverId};
use crate::secret::Secret;
use crate::store::{SecretError, Secrets};

/// The AEAD key length (ChaCha20-Poly1305: 256-bit).
const KEY_LEN: usize = 32;
/// The AEAD nonce length (96-bit).
const NONCE_LEN: usize = 12;
/// Magic + version prefix on the on-disk blob, so a format change is detectable.
const MAGIC: &[u8] = b"QFSSEC01";

/// One stored credential entry as it sits inside the (decrypted) plaintext map: the raw
/// secret bytes + the metadata timestamp. Serialized as part of the cleartext that is
/// then AEAD-sealed; never written in the clear.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    /// Raw secret bytes. Lives in the cleartext map only transiently between decrypt and
    /// re-wrap; the on-disk form is always ciphertext.
    secret: Vec<u8>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// The whole decrypted credential map (`driver/account -> entry`). This is the plaintext
/// that gets AEAD-sealed into the single blob.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Vault {
    entries: BTreeMap<String, StoredEntry>,
}

/// The native encrypted-at-rest credential store.
pub struct LocalStore {
    path: PathBuf,
    key: Key,
    /// Serializes concurrent put/remove so the read-modify-write of the blob is atomic
    /// at the process level (the rename is atomic at the fs level).
    lock: Mutex<()>,
}

impl LocalStore {
    /// Open (or lazily create on first `put`) a store at `path` with an explicit 32-byte
    /// AEAD key — the OS-keyring path: the caller fetches the key from the keyring and
    /// hands it in. If the file exists, its mode is re-verified (`0600`, owner-only).
    ///
    /// # Errors
    /// [`SecretError::Backend`] if an existing file is group/world-readable (rejected) or
    /// its directory cannot be prepared.
    pub fn open_with_key(
        path: impl Into<PathBuf>,
        key: [u8; KEY_LEN],
    ) -> Result<Self, SecretError> {
        let path = path.into();
        if path.exists() {
            verify_owner_only(&path)?;
        }
        Ok(Self {
            path,
            key: Key::from(key),
            lock: Mutex::new(()),
        })
    }

    /// Open a store whose AEAD key is derived from a passphrase via argon2id + a fixed
    /// per-store salt argument. The salt is stored alongside (caller-managed) so the same
    /// passphrase reproduces the key; this is the no-keyring fallback.
    ///
    /// # Errors
    /// [`SecretError::Backend`] if key derivation fails or an existing file is not `0600`.
    pub fn from_passphrase(
        path: impl Into<PathBuf>,
        passphrase: &Secret,
        salt: &[u8],
    ) -> Result<Self, SecretError> {
        let key = derive_key(passphrase.expose(), salt)?;
        Self::open_with_key(path, key)
    }

    /// Read + decrypt the vault from disk. A missing file is an empty vault (not an error)
    /// so the first `put` creates it.
    fn load(&self) -> Result<Vault, SecretError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vault::default()),
            Err(e) => {
                return Err(SecretError::Backend(format!(
                    "reading credential blob: {e}"
                )))
            }
        };
        // Re-verify perms on every open (defense in depth against a chmod after create).
        verify_owner_only(&self.path)?;
        self.decrypt(&bytes)
    }

    /// Encrypt + atomically write the vault: write a temp file with `0600`, fsync, then
    /// `rename` over the target. A crash before the rename leaves the prior blob intact.
    fn store(&self, vault: &Vault) -> Result<(), SecretError> {
        let plaintext = serde_json::to_vec(vault)
            .map_err(|e| SecretError::Backend(format!("encoding vault: {e}")))?;
        let sealed = self.encrypt(&plaintext)?;

        let dir = self
            .path
            .parent()
            .ok_or_else(|| SecretError::Backend("credential path has no parent dir".into()))?;
        prepare_dir(dir)?;

        let tmp = self.path.with_extension("tmp");
        write_owner_only(&tmp, &sealed)?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            // Best-effort cleanup of the temp file on a failed rename.
            let _ = std::fs::remove_file(&tmp);
            SecretError::Backend(format!("atomic rename of credential blob: {e}"))
        })?;
        Ok(())
    }

    /// AEAD-seal: `MAGIC || nonce || ciphertext`. A fresh random nonce per write.
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError> {
        let cipher = ChaCha20Poly1305::new(&self.key);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| SecretError::Backend("sealing credential blob".into()))?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// AEAD-open. A wrong key or tampered blob fails authentication -> a secret-free
    /// error (we deliberately do not echo any bytes).
    fn decrypt(&self, blob: &[u8]) -> Result<Vault, SecretError> {
        let rest = blob
            .strip_prefix(MAGIC)
            .ok_or_else(|| SecretError::Backend("credential blob has unknown format".into()))?;
        if rest.len() < NONCE_LEN {
            return Err(SecretError::Backend("credential blob truncated".into()));
        }
        let (nonce_bytes, ct) = rest.split_at(NONCE_LEN);
        let cipher = ChaCha20Poly1305::new(&self.key);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext = cipher
            .decrypt(nonce, ct)
            // Wrong key or tamper: report locked (key mismatch) without leaking bytes.
            .map_err(|_| SecretError::Locked)?;
        serde_json::from_slice(&plaintext)
            .map_err(|e| SecretError::Backend(format!("decoding vault: {e}")))
    }
}

impl Secrets for LocalStore {
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        let _g = self.lock.lock();
        let vault = self.load()?;
        match vault.entries.get(&key.flat()) {
            Some(entry) => Ok(Secret::new(entry.secret.clone())),
            None => Err(SecretError::NotFound(key.clone())),
        }
    }

    fn put(&self, key: &CredentialKey, value: Secret) -> Result<(), SecretError> {
        let _g = self.lock.lock();
        let mut vault = self.load()?;
        vault.entries.insert(
            key.flat(),
            StoredEntry {
                secret: value.expose().to_vec(),
                created_at: OffsetDateTime::now_utc(),
            },
        );
        self.store(&vault)
    }

    fn remove(&self, key: &CredentialKey) -> Result<(), SecretError> {
        let _g = self.lock.lock();
        let mut vault = self.load()?;
        if vault.entries.remove(&key.flat()).is_some() {
            self.store(&vault)?;
        }
        Ok(())
    }

    fn list(&self, driver: Option<&DriverId>) -> Result<Vec<AccountRecord>, SecretError> {
        let _g = self.lock.lock();
        let vault = self.load()?;
        let mut out = Vec::new();
        for (flat, entry) in &vault.entries {
            let Some((drv, acct)) = flat.split_once('/') else {
                continue;
            };
            let Ok(account) = AccountId::new(acct) else {
                continue;
            };
            let rec = AccountRecord::new(DriverId::new(drv), account, entry.created_at);
            if driver.is_none_or(|d| &rec.driver == d) {
                out.push(rec);
            }
        }
        Ok(out)
    }
}

/// Derive a 32-byte AEAD key from a passphrase with argon2id.
fn derive_key(passphrase: &[u8], salt: &[u8]) -> Result<[u8; KEY_LEN], SecretError> {
    use argon2::Argon2;
    let argon = Argon2::default();
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase, salt, &mut key)
        .map_err(|_| SecretError::Backend("deriving key from passphrase".into()))?;
    Ok(key)
}

/// The XDG default path for the credential blob (`$XDG_CONFIG_HOME/qfs/credentials`,
/// falling back to `~/.config/qfs/credentials`). Returns `None` if neither env var is set.
#[must_use]
pub fn default_credentials_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("qfs").join("credentials"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("qfs")
                .join("credentials")
        })
}

/// Load the per-store KDF salt at `path`, creating a fresh random 16-byte salt (written `0600`,
/// parent dir created) on first use. The salt must persist so the same passphrase reproduces the
/// AEAD key — `LocalStore::from_passphrase` takes it back. Kept here (with the perm helpers + the
/// `rand` dep) so the credential plumbing stays in one crate.
///
/// # Errors
/// [`SecretError::Backend`] on an I/O failure or a too-short existing salt file.
pub fn load_or_create_salt(path: &Path) -> Result<Vec<u8>, SecretError> {
    match std::fs::read(path) {
        Ok(b) if b.len() >= 8 => Ok(b),
        Ok(_) => Err(SecretError::Backend(
            "credential salt file is too short (corrupt?)".into(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut salt = [0u8; 16];
            rand::rng().fill_bytes(&mut salt);
            if let Some(dir) = path.parent() {
                prepare_dir(dir)?;
            }
            write_owner_only(path, &salt)?;
            Ok(salt.to_vec())
        }
        Err(e) => Err(SecretError::Backend(format!(
            "reading credential salt: {e}"
        ))),
    }
}

// ---- POSIX permission helpers (owner-only 0600) -------------------------------------

#[cfg(unix)]
fn verify_owner_only(path: &Path) -> Result<(), SecretError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .map_err(|e| SecretError::Backend(format!("stat credential blob: {e}")))?;
    let mode = meta.permissions().mode() & 0o077;
    if mode != 0 {
        return Err(SecretError::Backend(format!(
            "credential blob is group/other-accessible (mode {:o}); refusing to use it",
            meta.permissions().mode() & 0o777
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_owner_only(_path: &Path) -> Result<(), SecretError> {
    // Non-POSIX hosts have no 0600 notion; the AEAD layer is the confidentiality guard.
    Ok(())
}

#[cfg(unix)]
fn prepare_dir(dir: &Path) -> Result<(), SecretError> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| SecretError::Backend(format!("creating credential dir: {e}")))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare_dir(dir: &Path) -> Result<(), SecretError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| SecretError::Backend(format!("creating credential dir: {e}")))
}

#[cfg(unix)]
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| SecretError::Backend(format!("opening temp credential file: {e}")))?;
    // `.mode()` only applies when the file is *created*; a pre-existing temp (e.g. a
    // leftover from a crash) keeps its old perms. Re-assert 0600 explicitly so the blob
    // that gets renamed into place is always owner-only.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| SecretError::Backend(format!("chmod temp credential file: {e}")))?;
    f.write_all(bytes)
        .map_err(|e| SecretError::Backend(format!("writing credential blob: {e}")))?;
    f.sync_all()
        .map_err(|e| SecretError::Backend(format!("fsync credential blob: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    let mut f = std::fs::File::create(path)
        .map_err(|e| SecretError::Backend(format!("opening temp credential file: {e}")))?;
    f.write_all(bytes)
        .map_err(|e| SecretError::Backend(format!("writing credential blob: {e}")))?;
    f.sync_all()
        .map_err(|e| SecretError::Backend(format!("fsync credential blob: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_bytes() -> [u8; KEY_LEN] {
        // A fixed test key (NOT a real credential; deterministic so the round-trip is
        // reproducible). Never used outside tests.
        [7u8; KEY_LEN]
    }

    fn ckey(driver: &str, account: &str) -> CredentialKey {
        CredentialKey::new(DriverId::new(driver), AccountId::new(account).unwrap())
    }

    /// Round-trip put -> get -> remove against a real encrypted blob in a tempdir.
    #[test]
    fn local_store_round_trips_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qfs").join("credentials");
        let store = LocalStore::open_with_key(&path, key_bytes()).unwrap();

        let k = ckey("mail", "work");
        assert_eq!(store.get(&k).unwrap_err().code(), "secret_not_found");

        store.put(&k, Secret::from("real-token-xyz")).unwrap();
        assert_eq!(store.get(&k).unwrap().expose_str(), Some("real-token-xyz"));

        // The on-disk blob is ciphertext: the plaintext token must NOT appear in it.
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows("real-token-xyz".len())
                .any(|w| w == b"real-token-xyz"),
            "plaintext token leaked into the on-disk blob"
        );

        store.remove(&k).unwrap();
        assert_eq!(store.get(&k).unwrap_err().code(), "secret_not_found");
    }

    /// The created file is mode 0600.
    #[cfg(unix)]
    #[test]
    fn created_blob_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let store = LocalStore::open_with_key(&path, key_bytes()).unwrap();
        store.put(&ckey("s3", "prod"), Secret::from("k")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "blob mode was {mode:o}, expected 600");
    }

    /// A group/world-readable blob is rejected on open.
    #[cfg(unix)]
    #[test]
    fn group_readable_blob_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let store = LocalStore::open_with_key(&path, key_bytes()).unwrap();
        store.put(&ckey("s3", "prod"), Secret::from("k")).unwrap();

        // Loosen perms behind the store's back, then a fresh open must reject.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();

        // `LocalStore` is intentionally NOT Debug (it holds a key), so match the Result
        // rather than call `unwrap_err` (which would require the Ok type to be Debug).
        match LocalStore::open_with_key(&path, key_bytes()) {
            Err(err) => {
                assert_eq!(err.code(), "secret_backend");
                assert!(err.to_string().contains("group/other-accessible"));
            }
            Ok(_) => panic!("expected open of a group-readable blob to be rejected"),
        }
    }

    /// Atomic write: a stray `.tmp` left by a crash between temp-write and rename does NOT
    /// corrupt the live blob — the prior blob is still decryptable and the temp is ignored.
    #[test]
    fn prior_blob_survives_a_dangling_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let store = LocalStore::open_with_key(&path, key_bytes()).unwrap();
        store
            .put(&ckey("mail", "work"), Secret::from("v1"))
            .unwrap();

        // Simulate a crash mid-write: a leftover temp file with garbage, never renamed.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, b"GARBAGE-not-renamed").unwrap();

        // The live blob is untouched and still decryptable.
        assert_eq!(
            store.get(&ckey("mail", "work")).unwrap().expose_str(),
            Some("v1")
        );
        // The next successful put cleanly replaces the blob (rename overwrites).
        store
            .put(&ckey("mail", "work"), Secret::from("v2"))
            .unwrap();
        assert_eq!(
            store.get(&ckey("mail", "work")).unwrap().expose_str(),
            Some("v2")
        );
    }

    /// A wrong key fails authenticated decryption -> Locked (no bytes leaked).
    #[test]
    fn wrong_key_is_locked_not_a_leak() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let store = LocalStore::open_with_key(&path, key_bytes()).unwrap();
        store
            .put(&ckey("mail", "work"), Secret::from("v1"))
            .unwrap();

        let wrong = LocalStore::open_with_key(&path, [9u8; KEY_LEN]).unwrap();
        let err = wrong.get(&ckey("mail", "work")).unwrap_err();
        assert_eq!(err.code(), "secret_locked");
    }

    /// Passphrase-derived key (argon2id) reproduces the same key for the same
    /// passphrase+salt, so a blob written under it round-trips.
    #[test]
    fn passphrase_derived_key_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let pass = Secret::from("correct horse battery staple");
        let salt = b"qfs-fixed-salt-16";

        let store = LocalStore::from_passphrase(&path, &pass, salt).unwrap();
        store
            .put(&ckey("gh", "main"), Secret::from("ghp_xxx"))
            .unwrap();

        let reopened = LocalStore::from_passphrase(&path, &pass, salt).unwrap();
        assert_eq!(
            reopened.get(&ckey("gh", "main")).unwrap().expose_str(),
            Some("ghp_xxx")
        );
    }

    #[test]
    fn list_filters_by_driver() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let store = LocalStore::open_with_key(&path, key_bytes()).unwrap();
        store.put(&ckey("mail", "work"), Secret::from("a")).unwrap();
        store.put(&ckey("mail", "home"), Secret::from("b")).unwrap();
        store.put(&ckey("s3", "prod"), Secret::from("c")).unwrap();

        assert_eq!(store.list(None).unwrap().len(), 3);
        assert_eq!(store.list(Some(&DriverId::new("mail"))).unwrap().len(), 2);
    }
}
