//! The object-storage (`/s3`, `/r2`) composition root: the operator config the binary injects into
//! the live SigV4 [`HttpBackend`](qfs_driver_objstore::HttpBackend), plus the cred-free planning
//! registries the shell/serve engine mounts so an `/s3`/`/r2` statement PLANS.
//!
//! `qfs-driver-objstore` is a `qfs-runtime` consumer that must stay a LEAF — only the terminal
//! binary may depend on it — so the operator config (endpoint / region / bucket / access-key id)
//! lives HERE and the live SigV4 backend is constructed + bridged into the interpreter from the
//! binary (`crate::commit`), exactly like the local / fs / sql / git composition. DESCRIBE is a
//! pure, cred-free facet (registered in the describe registry); the live signed-HTTP backend is
//! built only on the apply registry.
//!
//! ## Config (no credentials) — fail closed by default
//! The **non-secret** routing config is read from the process environment; the **secret access
//! key** is resolved from the encrypted credential store (`crate::commit`), keyed by the driver id
//! (`s3` / `r2`) and the MOUNT's account label — exactly the secret `qfs account add s3 <label>`
//! sealed (or the `QFS_SECRET_S3_<NAME>` env fallback the agent/CI path reads). With ANY required
//! routing var absent the config is `None` and the driver is left UNREGISTERED — a `/s3` commit then
//! fails closed ("no driver / not configured"), never a silent or faked write.
//!
//! - **S3**: `QFS_S3_REGION` (e.g. `us-east-1`), `QFS_S3_ACCESS_KEY_ID`, `QFS_S3_BUCKET`, and the
//!   optional `QFS_S3_ENDPOINT` (defaults to the virtual-host-free `https://s3.<region>.amazonaws.com`).
//! - **R2**: `QFS_R2_ACCOUNT_ID`, `QFS_R2_ACCESS_KEY_ID`, `QFS_R2_BUCKET`, and the optional
//!   `QFS_R2_ENDPOINT` (defaults to `https://<account>.r2.cloudflarestorage.com`); the SigV4 region
//!   for R2 is the fixed sentinel `auto`.
//!
//! The access key id is NON-secret config (it appears in the SigV4 `Authorization` credential scope
//! on the wire); only the secret access key is a [`qfs_secrets::Secret`], envelope-encrypted at rest
//! and exposed only transiently inside the signer.
//!
//! ## One bucket per mount (a documented seam)
//! The [`ObjRegistry`](qfs_driver_objstore::ObjRegistry) resolves a backend by the path's `<bucket>`
//! segment, so the live registry registers the ONE operator-configured bucket
//! (`QFS_S3_BUCKET`/`QFS_R2_BUCKET`); a commit addressing any other bucket fails closed ("no such
//! registered bucket"). On-demand multi-bucket binding is a future extension behind the same seam.

use std::sync::Arc;

use qfs_driver_objstore::{Bucket, Endpoint, MockObjectBackend, ObjRegistry, Scheme};

/// The resolved **non-secret** routing config for one object-storage scheme (S3 / R2): the
/// SigV4 [`Endpoint`] (base URL + region), the registered bucket name, and the access key id (which
/// rides in the `Authorization` credential scope on the wire — never the secret access key, which is
/// resolved separately from the encrypted store).
#[derive(Debug, Clone)]
pub struct ObjConfig {
    /// The endpoint (base URL + SigV4 region) the backend routes with.
    pub endpoint: Endpoint,
    /// The single bucket name registered into the live [`ObjRegistry`].
    pub bucket: String,
    /// The AWS/R2 access key id (non-secret; appears in the SigV4 credential scope).
    pub access_key_id: String,
}

/// Read one non-empty environment variable, treating an empty value as absent (the deny/fail-closed
/// convention shared with `crate::fs` / `crate::sql`).
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Resolve the live **S3** routing config from the environment, or `None` (fail closed) when any
/// required var (`QFS_S3_REGION` / `QFS_S3_ACCESS_KEY_ID` / `QFS_S3_BUCKET`) is absent. The endpoint
/// defaults to the path-style `https://s3.<region>.amazonaws.com` unless `QFS_S3_ENDPOINT` overrides
/// it (an S3-compatible endpoint, e.g. MinIO).
#[must_use]
pub fn s3_config() -> Option<ObjConfig> {
    let region = env_nonempty("QFS_S3_REGION")?;
    let access_key_id = env_nonempty("QFS_S3_ACCESS_KEY_ID")?;
    let bucket = env_nonempty("QFS_S3_BUCKET")?;
    let base_url = env_nonempty("QFS_S3_ENDPOINT")
        .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
    Some(ObjConfig {
        endpoint: Endpoint::new(base_url, region),
        bucket,
        access_key_id,
    })
}

/// Resolve the live **R2** routing config from the environment, or `None` (fail closed) when any
/// required var (`QFS_R2_ACCOUNT_ID` / `QFS_R2_ACCESS_KEY_ID` / `QFS_R2_BUCKET`) is absent. The
/// endpoint defaults to `https://<account>.r2.cloudflarestorage.com` unless `QFS_R2_ENDPOINT`
/// overrides it; R2's SigV4 region is the fixed sentinel `auto`.
#[must_use]
pub fn r2_config() -> Option<ObjConfig> {
    let account = env_nonempty("QFS_R2_ACCOUNT_ID")?;
    let access_key_id = env_nonempty("QFS_R2_ACCESS_KEY_ID")?;
    let bucket = env_nonempty("QFS_R2_BUCKET")?;
    let base_url = env_nonempty("QFS_R2_ENDPOINT")
        .unwrap_or_else(|| format!("https://{account}.r2.cloudflarestorage.com"));
    Some(ObjConfig {
        endpoint: Endpoint::new(base_url, "auto"),
        bucket,
        access_key_id,
    })
}

/// The config for `scheme`, used by the planning-mount builder to also register the live bucket name.
#[must_use]
fn scheme_config(scheme: Scheme) -> Option<ObjConfig> {
    match scheme {
        Scheme::S3 => s3_config(),
        Scheme::R2 => r2_config(),
    }
}

/// The current-UTC SigV4 signing timestamps `(amz_date, date_stamp)` — `YYYYMMDDTHHMMSSZ` and
/// `YYYYMMDD`. The [`HttpBackend`](qfs_driver_objstore::HttpBackend) fixes these at construction (a
/// deterministic-signing seam); the live registry is rebuilt per short-lived commit invocation, so
/// constructing with the current wall clock here is correct for that one commit. Formatted from the
/// `time` crate's calendar fields directly (no `macros` feature needed); a SigV4 signature is only
/// valid within a ~15-minute clock skew of the request, which a per-commit build comfortably meets.
#[must_use]
pub fn current_signing_dates() -> (String, String) {
    let now = time::OffsetDateTime::now_utc();
    let date_stamp = format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    let amz_date = format!(
        "{date_stamp}T{:02}{:02}{:02}Z",
        now.hour(),
        now.minute(),
        now.second()
    );
    (amz_date, date_stamp)
}

/// Build the **cred-free** planning [`ObjRegistry`] for `scheme`: a representative `bucket` (the
/// describe convention — so the `qfs describe` / driver-catalog goldens and a basic `/s3/bucket/<key>`
/// plan resolve) plus the operator-configured live bucket name when present (so a real
/// `/<scheme>/<configured-bucket>/<key>` statement also PLANS). Every bucket rides a
/// [`MockObjectBackend`] that is **never applied** — planning reads only the pure introspective half
/// (describe/capabilities/pushdown), never `Driver::applier`; the real SigV4 backend that APPLIES is
/// built on the apply registry (`crate::commit`), keyed by the same driver id. No credential, no
/// socket, no network.
#[must_use]
pub fn planning_registry(scheme: Scheme) -> ObjRegistry {
    let mut reg =
        ObjRegistry::new().with_bucket("bucket", Bucket::new(Arc::new(MockObjectBackend::new())));
    if let Some(cfg) = scheme_config(scheme) {
        if cfg.bucket != "bucket" {
            reg = reg.with_bucket(cfg.bucket, Bucket::new(Arc::new(MockObjectBackend::new())));
        }
    }
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SigV4 timestamps have the exact basic-ISO8601 shapes the signer requires: `YYYYMMDD`
    /// (8 digits) and `YYYYMMDDTHHMMSSZ` (the same date stamp + `T` + `HHMMSS` + `Z`), and the
    /// `amz_date` begins with the `date_stamp` (the SigV4 credential-scope/date coupling).
    #[test]
    fn signing_dates_have_the_basic_iso8601_shapes() {
        let (amz_date, date_stamp) = current_signing_dates();
        assert_eq!(date_stamp.len(), 8, "date stamp is YYYYMMDD: {date_stamp}");
        assert!(date_stamp.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(
            amz_date.len(),
            16,
            "amz date is YYYYMMDDTHHMMSSZ: {amz_date}"
        );
        assert!(amz_date.starts_with(&date_stamp));
        assert_eq!(amz_date.as_bytes()[8], b'T');
        assert!(amz_date.ends_with('Z'));
    }

    /// The planning registry always carries the representative `bucket` so a `/s3/bucket/<key>`
    /// statement resolves its per-node capabilities (the parse-time gate keys off a *registered*
    /// bucket) and PLANS — independent of any live config. Hermetic: builds the registry directly.
    #[test]
    fn planning_registry_registers_the_representative_bucket() {
        assert!(planning_registry(Scheme::S3).has_bucket("bucket"));
        assert!(planning_registry(Scheme::R2).has_bucket("bucket"));
    }
}
