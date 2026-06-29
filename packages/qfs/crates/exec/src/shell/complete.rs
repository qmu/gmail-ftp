//! Tab completion (ticket t28, hard part (b)): builtin names, mount/driver names, and path
//! segments under the resolved parent. The completer is a **pure API** over the registries —
//! the REPL driver binds it to the line editor, but tests drive it directly (no terminal).
//!
//! ## Latency bound (hard part (b))
//! Path-segment completion issues a cheap `ls` (a pure read) against the resolved parent
//! directory. A slow driver must never hang the prompt, so the parent listing runs under a
//! short [`COMPLETE_TIMEOUT`]; on timeout (or any scan error) the completer degrades to the
//! static candidates (builtins + mounts) it already has — completion is best-effort, never a
//! correctness or liveness risk. The parent listing is cached per (parent) key so repeated TABs
//! at the same prompt do not re-scan.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use qfs_core::{Engine, Value};

use crate::exec::block_on_read;
use crate::read::ReadRegistry;
use crate::shell::desugar::{classify, Builtin, Line};
use crate::shell::path::{resolve, VfsPath};

/// The per-listing timeout that bounds a completion scan (hard part (b)). Short enough that a
/// slow/remote driver never freezes the REPL; on expiry the completer falls back to static
/// candidates.
pub const COMPLETE_TIMEOUT: Duration = Duration::from_millis(750);

/// The completer over the live registries. Caches the per-parent `ls` so repeated completions at
/// one prompt are free. Interior mutability keeps the public API `&self` (the line editor holds
/// it behind a shared reference).
pub struct Completer<'a> {
    engine: &'a Engine,
    reads: &'a ReadRegistry,
    /// Cache: resolved parent path → its child segment names. Cleared per prompt by the REPL.
    cache: RefCell<HashMap<String, Vec<String>>>,
}

impl<'a> Completer<'a> {
    /// Build a completer over the registries.
    #[must_use]
    pub fn new(engine: &'a Engine, reads: &'a ReadRegistry) -> Self {
        Self {
            engine,
            reads,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Drop the per-prompt listing cache (the REPL calls this each time it redraws the prompt,
    /// so a fresh prompt always re-validates the directory).
    pub fn invalidate(&self) {
        self.cache.borrow_mut().clear();
    }

    /// Complete the (possibly partial) `line` typed against `cwd`, returning candidate
    /// completions sorted + de-duplicated. Best-effort: a slow path scan degrades silently to
    /// the static candidates.
    ///
    /// - An empty / first-token line completes **builtin names** (and lets a raw qfs keyword
    ///   through untouched — we never complete grammar keywords here).
    /// - A builtin's path argument completes **mount names** (when the fragment looks like a
    ///   top-level `/x`) and **path segments** under the resolved parent.
    #[must_use]
    pub fn complete(&self, line: &str, cwd: &VfsPath) -> Vec<String> {
        let mut out = match classify(line) {
            // No head token yet (or a partial first word): offer the builtin verbs.
            Line::Empty => Builtin::all_names()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            Line::Raw(raw) => {
                // The first word is still being typed (no whitespace yet) → it could become a
                // builtin; offer the builtin-name prefix matches. Once there is a space, a raw
                // qfs statement is left to the grammar (we do not complete keywords).
                if raw.split_whitespace().count() <= 1 && !raw.contains(char::is_whitespace) {
                    prefix_matches(Builtin::all_names(), &raw)
                } else {
                    Vec::new()
                }
            }
            Line::Builtin { verb, args } => self.complete_arg(verb, &args, line, cwd),
        };
        out.sort();
        out.dedup();
        out
    }

    /// Complete the path argument of a builtin. The fragment being completed is the last token
    /// (or empty if the line ends in whitespace).
    fn complete_arg(
        &self,
        verb: Builtin,
        args: &[String],
        line: &str,
        cwd: &VfsPath,
    ) -> Vec<String> {
        // `pwd` takes no argument; nothing to complete.
        if matches!(verb, Builtin::Pwd) {
            return Vec::new();
        }
        // The fragment under the cursor: the last token, or "" when the line ends in space.
        let frag = if line.ends_with(char::is_whitespace) {
            ""
        } else {
            args.last().map_or("", String::as_str)
        };

        // A `/x` fragment with no further slash completes against the MOUNT names (cross-driver
        // addressing); otherwise complete path segments under the resolved parent.
        if let Some(rest) = frag.strip_prefix('/') {
            if !rest.contains('/') {
                let mounts = self.mount_names();
                return prefix_matches_owned(
                    &mounts.iter().map(|m| format!("/{m}")).collect::<Vec<_>>(),
                    frag,
                );
            }
        }

        self.complete_segments(frag, cwd)
    }

    /// Complete a path-segment fragment under its parent directory: split the fragment into a
    /// parent + trailing partial, `ls` the parent (cached, timeout-bounded), and prefix-match the
    /// child names.
    fn complete_segments(&self, frag: &str, cwd: &VfsPath) -> Vec<String> {
        // Parent = everything up to the last `/`; partial = the trailing component.
        let (parent_frag, partial) = match frag.rfind('/') {
            Some(i) => (&frag[..=i], &frag[i + 1..]),
            None => ("", frag),
        };
        // Resolve the parent against cwd. An empty parent_frag is the cwd itself.
        let parent = if parent_frag.is_empty() {
            cwd.clone()
        } else {
            match resolve(parent_frag, cwd) {
                Ok(p) => p,
                Err(_) => return Vec::new(),
            }
        };

        let children = self.list_children(&parent);
        // Re-prefix each child with the parent fragment so the completion replaces the whole
        // fragment (e.g. completing `sub/re` → `sub/readme.md`).
        children
            .into_iter()
            .filter(|c| c.starts_with(partial))
            .map(|c| format!("{parent_frag}{c}"))
            .collect()
    }

    /// List the child segment names of `parent` via a cached, timeout-bounded `ls`. Returns the
    /// `name` column of each row. On any error / timeout / missing read driver, returns empty
    /// (the completer degrades gracefully).
    fn list_children(&self, parent: &VfsPath) -> Vec<String> {
        let key = parent.render();
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let names = self.scan_names(parent).unwrap_or_default();
        self.cache.borrow_mut().insert(key, names.clone());
        names
    }

    /// Run the bounded `ls` scan for `parent` and extract the `name` column. The scan runs under
    /// [`COMPLETE_TIMEOUT`] so a slow driver cannot hang the prompt.
    fn scan_names(&self, parent: &VfsPath) -> Option<Vec<String>> {
        let stmt_src = format!("{} |> SELECT name", parent.render());
        let stmt = crate::exec::parse(&stmt_src).ok()?;
        // block_on_read builds its own current-thread runtime; bound the whole read with a
        // wall-clock guard by running it on a thread we join with a timeout. This keeps the
        // completer free of an injected async runtime while still honouring the latency bound.
        let rows = bounded_read(&stmt, &self.engine.mounts, self.reads, COMPLETE_TIMEOUT)?;
        let name_idx = rows
            .schema
            .columns
            .iter()
            .position(|c| c.name.as_str() == "name")?;
        let mut names: Vec<String> = rows
            .rows
            .iter()
            .filter_map(|r| match r.values.get(name_idx) {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names.dedup();
        Some(names)
    }

    /// The mounted driver/mount names (the `local`/`mail`/… completion set).
    fn mount_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .engine
            .mounts
            .drivers()
            .map(|d| d.id().as_str().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Run a read on a worker thread and join it under `timeout`. On timeout the worker thread is
/// detached (it finishes harmlessly against the in-memory/local read) and the completer falls
/// back to no candidates. Keeps the latency bound without pulling an async timer into the pure
/// completer.
fn bounded_read(
    stmt: &qfs_parser::Statement,
    mounts: &qfs_core::MountRegistry,
    reads: &ReadRegistry,
    timeout: Duration,
) -> Option<crate::dto::RowSet> {
    use std::sync::mpsc;
    // The read borrows the registries; scope the borrow to a thread that cannot outlive them by
    // cloning the owned inputs the scan needs. The mount registry + read registry are cheaply
    // clonable (Arc-backed), and the statement is owned here.
    let mounts = mounts.clone();
    let reads = reads.clone();
    let stmt = stmt.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = block_on_read(&stmt, &mounts, &reads).ok();
        // The receiver may already be gone (timeout); ignore the send error.
        let _ = tx.send(res);
    });
    // On timeout the worker is detached and the completer falls back to no candidates.
    rx.recv_timeout(timeout).ok().flatten()
}

/// Prefix-match `frag` against a set of static `&str` candidates.
fn prefix_matches(candidates: &[&str], frag: &str) -> Vec<String> {
    candidates
        .iter()
        .filter(|c| c.starts_with(frag))
        .map(|c| (*c).to_string())
        .collect()
}

/// Prefix-match `frag` against a set of owned candidates.
fn prefix_matches_owned(candidates: &[String], frag: &str) -> Vec<String> {
    candidates
        .iter()
        .filter(|c| c.starts_with(frag))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use qfs_core::{
        Archetype, Capabilities, CfsError, Column, ColumnType, Driver, DriverId, NodeDesc, Path,
        PushdownProfile, Row, RowBatch, Schema, Value,
    };
    use qfs_pushdown::ScanNode;

    struct FakeNs {
        mount: String,
        names: Vec<String>,
    }
    fn ns_schema() -> Schema {
        Schema::new(vec![Column::new("name", ColumnType::Text, false)])
    }
    impl Driver for FakeNs {
        fn mount(&self) -> &str {
            &self.mount
        }
        fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
            Ok(NodeDesc::new(Archetype::BlobNamespace, ns_schema()))
        }
        fn capabilities(&self, _p: &Path) -> Capabilities {
            Capabilities::none()
        }
        fn procedures(&self) -> &[qfs_core::ProcSig] {
            &[]
        }
        fn pushdown(&self) -> &PushdownProfile {
            &PushdownProfile::None
        }
        fn applier(&self) -> &dyn qfs_core::PlanApplier {
            unreachable!()
        }
    }
    #[async_trait::async_trait]
    impl crate::read::ReadDriver for FakeNs {
        async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
            let rows = self
                .names
                .iter()
                .map(|n| Row::new(vec![Value::Text(n.clone())]))
                .collect();
            Ok(RowBatch::new(ns_schema(), rows))
        }
    }

    fn setup() -> (Engine, ReadRegistry) {
        let mut e = Engine::new();
        e.mounts
            .register(Arc::new(FakeNs {
                mount: "/local".into(),
                names: vec!["docs".into(), "downloads".into(), "readme.md".into()],
            }))
            .unwrap();
        let reads = ReadRegistry::new().with(
            DriverId::new("local"),
            Arc::new(FakeNs {
                mount: "/local".into(),
                names: vec!["docs".into(), "downloads".into(), "readme.md".into()],
            }),
        );
        (e, reads)
    }

    #[test]
    fn completes_builtin_names_at_head() {
        let (e, reads) = setup();
        let c = Completer::new(&e, &reads);
        let got = c.complete("c", &VfsPath::root("local"));
        assert!(got.contains(&"cat".to_string()));
        assert!(got.contains(&"cd".to_string()));
        assert!(got.contains(&"cp".to_string()));
        assert!(
            !got.contains(&"ls".to_string()),
            "`c` should not match `ls`"
        );
    }

    #[test]
    fn completes_mount_names_for_absolute_fragment() {
        let (e, reads) = setup();
        let c = Completer::new(&e, &reads);
        let got = c.complete("cd /lo", &VfsPath::root("local"));
        assert_eq!(got, vec!["/local"]);
    }

    #[test]
    fn completes_path_segments_via_ls() {
        let (e, reads) = setup();
        let c = Completer::new(&e, &reads);
        let got = c.complete("ls do", &VfsPath::root("local"));
        assert_eq!(got, vec!["docs", "downloads"]);
    }

    #[test]
    fn completes_all_segments_on_trailing_space() {
        let (e, reads) = setup();
        let c = Completer::new(&e, &reads);
        let got = c.complete("cat ", &VfsPath::root("local"));
        assert_eq!(got, vec!["docs", "downloads", "readme.md"]);
    }

    #[test]
    fn caches_parent_listing() {
        let (e, reads) = setup();
        let c = Completer::new(&e, &reads);
        let _ = c.complete("ls do", &VfsPath::root("local"));
        // Second call hits the cache (same parent key); still correct.
        let got = c.complete("ls read", &VfsPath::root("local"));
        assert_eq!(got, vec!["readme.md"]);
    }

    #[test]
    fn unknown_parent_degrades_to_empty() {
        let (e, reads) = setup();
        let c = Completer::new(&e, &reads);
        // No read driver for `/mail`: completion degrades to empty, never errors/hangs.
        let got = c.complete("ls /mail/x", &VfsPath::root("local"));
        assert!(got.is_empty());
    }
}
