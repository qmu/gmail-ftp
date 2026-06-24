//! The local driver's **read scan** (ticket t28): translate a VFS path + the pushed query into
//! the owned listing [`RowBatch`] the read executor consumes. This is the read counterpart of
//! the [`applier`](crate::applier) write leg — the natural first consumer of the t29 read seam.
//!
//! It owns no async and no `qfs-exec` dependency (keeping the driver crate off the integration
//! layer per the t29 topology guard): it is a pure, synchronous `VFS path -> RowBatch` over the
//! existing [`fs_core`](crate::fs_core) scan primitives. The async `qfs_exec::ReadDriver` adapter
//! that drives this lives in the **`qfs` binary crate** (`crates/qfs/src/shell.rs`) — the
//! shell's composition root. It cannot live in `qfs-cmd`: this driver crate is a `qfs-runtime`
//! consumer, so a `qfs-cmd -> qfs-driver-local` edge would make qfs-cmd a non-leaf runtime
//! consumer and trip the runtime-leaf-confinement guard. The binary is the one node that is BOTH
//! an allowlisted runtime consumer AND a terminal sink (nothing depends on it, so tokio
//! dead-ends there), which is why the adapter lives there and the confinement guards stay green.
//!
//! ## Pushdown honesty (t20)
//! The local driver declares `Partial { project: true }`: it can honour a projection but not a
//! `WHERE`/`LIMIT`. This scan therefore returns the **full listing** (over-returning is allowed)
//! and lets the executor's residual re-filter trim it — exactly the t20 property. We do apply the
//! projection when the pushed query carries one (cheap, and it keeps the returned schema honest),
//! but correctness never depends on it.

use qfs_types::{Name, Row, RowBatch, Schema, Value};

use crate::error::LocalError;
use crate::fs_core::{self, Sandbox};
use crate::row::LocalRow;

/// Scan `vfs` into the owned listing [`RowBatch`] (the `LocalRow` schema), optionally narrowed
/// to `project` columns. Dispatches on the path shape:
/// - a **glob** (`*`/`?`/`**`) → [`fs_core::resolve_glob`] matches;
/// - the **mount root or an existing directory** → [`fs_core::scan_dir`] (one level);
/// - otherwise a **single entry** (a file, or a non-existent path → empty).
///
/// Over-returns relative to any unpushable predicate/limit (the executor's residual trims it).
///
/// # Errors
/// [`LocalError`] on a sandbox escape or an I/O failure (a missing path is **not** an error — it
/// yields an empty batch, so a scan over a partially-present tree is robust).
pub fn scan_rows(
    sandbox: &Sandbox,
    vfs: &str,
    project: Option<&[Name]>,
) -> Result<RowBatch, LocalError> {
    let local_rows = scan_local_rows(sandbox, vfs)?;
    let full = LocalRow::schema();
    let rows: Vec<Row> = local_rows.iter().map(LocalRow::to_row).collect();
    let batch = RowBatch::new(full, rows);
    Ok(match project {
        Some(cols) if !cols.is_empty() => project_batch(&batch, cols),
        _ => batch,
    })
}

/// Resolve `vfs` to its listing [`LocalRow`]s (the shape-dispatch above), pure over `fs_core`.
fn scan_local_rows(sandbox: &Sandbox, vfs: &str) -> Result<Vec<LocalRow>, LocalError> {
    if vfs.contains(['*', '?']) {
        return fs_core::resolve_glob(sandbox, vfs);
    }
    // Probe the resolved path: a directory lists its children; a file lists itself.
    let abs = sandbox.resolve(vfs)?;
    match abs.symlink_metadata() {
        Ok(meta) if meta.is_dir() => fs_core::scan_dir(sandbox, vfs),
        Ok(_) => {
            // A single existing file: return exactly its row (glob with no wildcard does this).
            fs_core::resolve_glob(sandbox, vfs)
        }
        // A path that does not exist yet yields an empty listing (robust, not an error).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(LocalError::from_io(vfs, &e)),
    }
}

/// Project `batch` onto the `cols` subset, preserving the requested column order. A requested
/// column absent from the listing schema is dropped (the executor's residual would reject an
/// impossible projection earlier; this stays total).
fn project_batch(batch: &RowBatch, cols: &[Name]) -> RowBatch {
    let src = &batch.schema;
    // Map each requested column to its index in the full listing schema.
    let picks: Vec<(usize, &qfs_types::Column)> = cols
        .iter()
        .filter_map(|name| {
            src.columns
                .iter()
                .position(|c| c.name.as_str() == name.as_str())
                .map(|i| (i, &src.columns[i]))
        })
        .collect();
    let schema = Schema::new(picks.iter().map(|(_, c)| (*c).clone()).collect());
    let rows = batch
        .rows
        .iter()
        .map(|r| {
            Row::new(
                picks
                    .iter()
                    .map(|(i, _)| r.values.get(*i).cloned().unwrap_or(Value::Null))
                    .collect(),
            )
        })
        .collect();
    RowBatch::new(schema, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Sandbox) {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.md"), b"alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"beta").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("c.md"), b"gamma").unwrap();
        let sandbox = Sandbox::new(dir.path().to_path_buf());
        (dir, sandbox)
    }

    #[test]
    fn scans_directory_listing() {
        let (_d, sandbox) = fixture();
        let batch = scan_rows(&sandbox, "/local", None).unwrap();
        let names: Vec<String> = batch
            .rows
            .iter()
            .filter_map(|r| match &r.values[0] {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["a.md", "b.txt", "sub"]);
        assert_eq!(batch.schema.columns.len(), 6, "full LocalRow schema");
    }

    #[test]
    fn scans_subdirectory() {
        let (_d, sandbox) = fixture();
        let batch = scan_rows(&sandbox, "/local/sub", None).unwrap();
        assert_eq!(batch.rows.len(), 1);
    }

    #[test]
    fn single_file_returns_one_row() {
        let (_d, sandbox) = fixture();
        let batch = scan_rows(&sandbox, "/local/a.md", None).unwrap();
        assert_eq!(batch.rows.len(), 1);
        match &batch.rows[0].values[0] {
            Value::Text(s) => assert_eq!(s, "a.md"),
            other => panic!("expected name text, got {other:?}"),
        }
    }

    #[test]
    fn missing_path_is_empty_not_error() {
        let (_d, sandbox) = fixture();
        let batch = scan_rows(&sandbox, "/local/nope", None).unwrap();
        assert!(batch.rows.is_empty());
    }

    #[test]
    fn projection_narrows_columns() {
        let (_d, sandbox) = fixture();
        let cols = vec![Name::from("name"), Name::from("size")];
        let batch = scan_rows(&sandbox, "/local", Some(&cols)).unwrap();
        assert_eq!(batch.schema.columns.len(), 2);
        assert_eq!(batch.schema.columns[0].name.as_str(), "name");
        assert_eq!(batch.schema.columns[1].name.as_str(), "size");
    }

    #[test]
    fn glob_matches_files() {
        let (_d, sandbox) = fixture();
        let batch = scan_rows(&sandbox, "/local/*.md", None).unwrap();
        assert_eq!(batch.rows.len(), 1, "only top-level a.md matches *.md");
    }

    #[test]
    fn sandbox_escape_is_error() {
        let (_d, sandbox) = fixture();
        let err = scan_rows(&sandbox, "/local/../../etc", None).unwrap_err();
        assert_eq!(err.code(), "outside_sandbox");
    }
}
