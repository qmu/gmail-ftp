//! Host-agnostic **binding-set derivation** (t36): turn a `/server` registry projection into the
//! owned [`BindingSet`] both hosts consume.
//!
//! This module is wasm-clean: it carries the cadence→crontab mapping and the mount-reference
//! scanner over **owned strings** only. The conversion from a live `cfs_server::ServerState` is
//! [`crate::from_server`] (gated behind `host-daemon`); this module is the pure derivation those
//! conversions feed, so the wasm core can derive a binding set from owned input without pulling
//! the tokio-bearing `cfs-server` crate.

use std::collections::BTreeSet;

use crate::dto::{
    BindingSet, EndpointBinding, JobBinding, Mount, NativeStoreBinding, NativeStoreKind,
    WatcherBinding, WebhookBinding,
};

/// Derive the CF Cron-Trigger `crontab` (5-field: min hour dom mon dow) for an `EVERY <interval>`
/// cadence. The interval grammar is the restricted form the t33 scheduler already parses
/// (`<n><unit>` where unit ∈ `m`/`h`/`d`). Unrecognised cadences fall back to `* * * * *`
/// (every-minute) — the safe over-fire (at-least-once is idempotent, RFD §6) rather than a panic.
///
/// - `Nm` → every N minutes  → `*/N * * * *`   (N clamped to 1..=59)
/// - `1h` → hourly           → `0 * * * *`
/// - `Nh` → every N hours    → `0 */N * * *`   (N clamped to 1..=23)
/// - `1d` → daily (midnight) → `0 0 * * *`
/// - `Nd` → every N days     → `0 0 */N * *`   (N clamped to 1..=31)
#[must_use]
pub fn cron_from_every(every: &str) -> String {
    let every = every.trim();
    let Some((num, unit)) = split_interval(every) else {
        return "* * * * *".to_string();
    };
    match unit {
        'm' => {
            let n = num.clamp(1, 59);
            if n == 1 {
                "* * * * *".to_string()
            } else {
                format!("*/{n} * * * *")
            }
        }
        'h' => {
            let n = num.clamp(1, 23);
            if n == 1 {
                "0 * * * *".to_string()
            } else {
                format!("0 */{n} * * *")
            }
        }
        'd' => {
            let n = num.clamp(1, 31);
            if n == 1 {
                "0 0 * * *".to_string()
            } else {
                format!("0 0 */{n} * *")
            }
        }
        _ => "* * * * *".to_string(),
    }
}

/// Split a `<n><unit>` interval into its numeric part and unit char. Returns `None` for an
/// unparseable cadence.
fn split_interval(every: &str) -> Option<(u32, char)> {
    let unit = every.chars().last()?;
    if !unit.is_ascii_alphabetic() {
        return None;
    }
    let digits = &every[..every.len() - unit.len_utf8()];
    let num: u32 = digits.trim().parse().ok()?;
    Some((num, unit.to_ascii_lowercase()))
}

/// Scan owned statement-source text for `/cf/d1/<db>`, `/cf/r2/<bucket>`, `/cf/kv/<ns>` mount
/// references and collect the native-store bindings they imply. The scan is name-only: it parses
/// the resource segment out of the mount path; it never reads a value or a token. De-duplicated +
/// sorted (the same DB referenced by two queries yields one binding).
///
/// Two source shapes are handled, so a body derives the same set however it is stored:
///   * the **canonical serialized spec** (the t31 stored body — a parsed AST as JSON, where a path
///     is a `"segments":[{"name":"cf"},{"name":"d1"},{"name":"<db>"},…]` array): every `segments`
///     array is walked and the leading `cf`/`d1`/`r2`/`kv` prefix reconstructed into a mount; and
///   * **raw source text** (a fixture / INSERT string column with a literal `/cf/d1/<db>` token).
///
/// The accepted mount shapes mirror the t-series CF driver (`/cf/d1/<db>/<table>` etc) AND the
/// ticket's bare `/d1`·`/r2`·`/kv` short forms.
fn scan_native_stores(sources: &[&str], out: &mut BTreeSet<NativeStoreBinding>) {
    for src in sources {
        // (1) Canonical serialized spec: walk every `segments` array in the JSON.
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(src) {
            scan_json_segments(&json, out);
        }
        // (2) Raw source text: literal `/cf/<kind>/<resource>` (or bare `/<kind>`) tokens.
        for token in src.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',') {
            if let Some(binding) = parse_mount_ref(token) {
                out.insert(binding);
            }
        }
    }
}

/// Recursively walk a canonical-spec JSON value, reconstructing a native-store mount from any
/// `"segments": [ { "name": ".." }, … ]` array whose leading segments are `cf`/`<kind>`/`<resource>`.
fn scan_json_segments(value: &serde_json::Value, out: &mut BTreeSet<NativeStoreBinding>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Array(segs)) = map.get("segments") {
                let names: Vec<&str> = segs
                    .iter()
                    .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
                    .collect();
                if let Some(binding) = binding_from_segment_names(&names) {
                    out.insert(binding);
                }
            }
            for v in map.values() {
                scan_json_segments(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                scan_json_segments(v, out);
            }
        }
        _ => {}
    }
}

/// Build a native-store binding from a path's ordered segment names (`["cf","d1","analytics",…]`
/// or `["d1","analytics",…]`). Returns `None` if the path is not a `/cf/<kind>` / `/<kind>` mount.
fn binding_from_segment_names(names: &[&str]) -> Option<NativeStoreBinding> {
    let mut it = names.iter().copied();
    let first = it.next()?;
    let kind_seg = if first == "cf" { it.next()? } else { first };
    let kind = match kind_seg {
        "d1" => NativeStoreKind::D1,
        "r2" => NativeStoreKind::R2,
        "kv" => NativeStoreKind::Kv,
        _ => return None,
    };
    let resource = it.next().unwrap_or("default").to_string();
    let mount = Mount::new(format!("/cf/{}/{}", kind.label(), resource));
    Some(NativeStoreBinding {
        kind,
        resource,
        mount,
    })
}

/// Parse a single whitespace-delimited token into a [`NativeStoreBinding`] if it is a `/cf/<kind>`
/// or bare `/<kind>` native-store mount reference. Returns `None` otherwise.
fn parse_mount_ref(token: &str) -> Option<NativeStoreBinding> {
    let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/');
    let path = trimmed.strip_prefix('/')?;
    let names: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    binding_from_segment_names(&names)
}

/// The owned, host-agnostic inputs a binding set is derived from — the projection a host extracts
/// from the t30 registry (or a test constructs directly). Each row carries only owned, vendor-free
/// strings; the conversion from `cfs_server::ServerState` (behind `host-daemon`) builds this.
#[derive(Debug, Clone, Default)]
pub struct DerivationInput {
    /// `(name, method, route, policy)` per endpoint, plus the endpoint's backing-query source.
    pub endpoints: Vec<EndpointInput>,
    /// `(name, every, policy)` per job, plus the job's plan source.
    pub jobs: Vec<JobInput>,
    /// `(name, route, secret_handle)` per webhook.
    pub webhooks: Vec<WebhookInput>,
    /// `(name, on, policy)` per trigger/watcher, plus the trigger's plan + predicate source.
    pub watchers: Vec<WatcherInput>,
}

/// Owned endpoint derivation input.
#[derive(Debug, Clone, Default)]
pub struct EndpointInput {
    /// Handler name.
    pub name: String,
    /// HTTP method (uppercased).
    pub method: String,
    /// Route path.
    pub route: String,
    /// Read-only policy handle.
    pub policy: Option<String>,
    /// The backing-query source (scanned for native-store refs).
    pub query_source: String,
}

/// Owned job derivation input.
#[derive(Debug, Clone, Default)]
pub struct JobInput {
    /// Job name.
    pub name: String,
    /// Raw `EVERY` interval.
    pub every: String,
    /// Attached policy handle.
    pub policy: Option<String>,
    /// The DO-plan source (scanned for native-store refs).
    pub plan_source: String,
}

/// Owned webhook derivation input.
#[derive(Debug, Clone, Default)]
pub struct WebhookInput {
    /// Webhook name.
    pub name: String,
    /// Inbound route.
    pub route: String,
    /// Signing-secret handle (empty for unsigned).
    pub secret_handle: String,
}

/// Owned watcher/trigger derivation input.
#[derive(Debug, Clone, Default)]
pub struct WatcherInput {
    /// Trigger name.
    pub name: String,
    /// Watched event/source.
    pub on: String,
    /// Attached policy handle.
    pub policy: Option<String>,
    /// The DO-plan source (scanned for native-store refs).
    pub plan_source: String,
    /// The `WHERE` predicate source (scanned for native-store refs).
    pub predicate_source: String,
}

/// Derive the host-agnostic [`BindingSet`] from owned [`DerivationInput`]. Pure + wasm-clean: it
/// maps each cadence to a crontab, derives each webhook's queue name, builds each watcher's cursor
/// key (lazily, via `WatcherBinding::cursor_key`), and scans every plan/query source for
/// native-store mount references (de-duplicated + sorted).
#[must_use]
pub fn derive_bindings(input: &DerivationInput) -> BindingSet {
    let mut set = BindingSet::new();
    let mut native: BTreeSet<NativeStoreBinding> = BTreeSet::new();

    for ep in &input.endpoints {
        set.endpoints.push(EndpointBinding {
            name: ep.name.clone(),
            method: ep.method.clone(),
            route: ep.route.clone(),
            policy: ep.policy.clone(),
        });
        scan_native_stores(&[ep.query_source.as_str()], &mut native);
    }
    for job in &input.jobs {
        set.jobs.push(JobBinding {
            name: job.name.clone(),
            every: job.every.clone(),
            cron: cron_from_every(&job.every),
            policy: job.policy.clone(),
        });
        scan_native_stores(&[job.plan_source.as_str()], &mut native);
    }
    for wh in &input.webhooks {
        set.webhooks.push(WebhookBinding {
            name: wh.name.clone(),
            route: wh.route.clone(),
            secret_handle: wh.secret_handle.clone(),
            queue: format!("{}-events", wh.name),
        });
    }
    for w in &input.watchers {
        set.watchers.push(WatcherBinding {
            name: w.name.clone(),
            on: w.on.clone(),
            policy: w.policy.clone(),
        });
        scan_native_stores(
            &[w.plan_source.as_str(), w.predicate_source.as_str()],
            &mut native,
        );
    }

    set.native_stores = native.into_iter().collect();
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_maps_to_crontab() {
        assert_eq!(cron_from_every("1h"), "0 * * * *");
        assert_eq!(cron_from_every("7d"), "0 0 */7 * *");
        assert_eq!(cron_from_every("1d"), "0 0 * * *");
        assert_eq!(cron_from_every("5m"), "*/5 * * * *");
        assert_eq!(cron_from_every("1m"), "* * * * *");
        assert_eq!(cron_from_every("2h"), "0 */2 * * *");
        // Unparseable cadence falls back to every-minute (safe over-fire, never a panic).
        assert_eq!(cron_from_every(""), "* * * * *");
        assert_eq!(cron_from_every("garbage"), "* * * * *");
    }

    #[test]
    fn mount_scan_finds_d1_r2_kv_by_name_only() {
        let mut out = BTreeSet::new();
        scan_native_stores(
            &[
                "FROM /cf/d1/analytics/events |> LIMIT 5",
                "CP /local/x /cf/r2/backups/obj",
                "INSERT INTO /kv/sessions",
            ],
            &mut out,
        );
        let v: Vec<_> = out.into_iter().collect();
        assert_eq!(v.len(), 3, "three distinct native stores: {v:?}");
        assert!(v.iter().any(|b| b.kind == NativeStoreKind::D1 && b.resource == "analytics"));
        assert!(v.iter().any(|b| b.kind == NativeStoreKind::R2 && b.resource == "backups"));
        assert!(v.iter().any(|b| b.kind == NativeStoreKind::Kv && b.resource == "sessions"));
    }

    #[test]
    fn mount_scan_dedups_repeated_refs() {
        let mut out = BTreeSet::new();
        scan_native_stores(
            &["FROM /cf/d1/db1/t1", "FROM /cf/d1/db1/t2"],
            &mut out,
        );
        assert_eq!(out.len(), 1, "same db referenced twice yields one binding");
    }

    #[test]
    fn binding_name_is_name_only_and_deterministic() {
        let b = NativeStoreBinding {
            kind: NativeStoreKind::D1,
            resource: "my-analytics".to_string(),
            mount: Mount::new("/cf/d1/my-analytics"),
        };
        assert_eq!(b.binding_name(), "D1_MY_ANALYTICS");
    }
}
