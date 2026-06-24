//! `exec::shell` — the interactive FTP-like shell logic (ticket t28, RFD §7).
//!
//! ## Where the logic lives, and why here
//! The shell is a thin convenience layer that adds **no new execution semantics**: every line
//! desugars to the SAME closed-core statements the one-shot path produces and routes through the
//! SAME `qfs-exec` pipeline (parse → plan → PREVIEW/COMMIT, or parse → scan → rows). So the
//! shell logic belongs in `qfs-exec` (the integration crate that already owns that pipeline),
//! NOT in `qfs-cmd` (which the t01 C4 guard keeps logic-free) and NOT in the spine. The thin
//! `qfs` bin / `qfs-cmd` dispatches into this module when invoked with no subcommand; it owns
//! only the line editor + history + render glue (a real terminal), keeping this module fully
//! testable by feeding scripted input and asserting plans + outcomes.
//!
//! ## The pieces (all dependency-free, terminal-free, unit/golden-testable)
//! - [`path`] — pure VFS path resolution against the cwd (relative / `..` / `~`/root /
//!   cross-driver-absolute); the t29 carry-over relaxation (relative paths resolve against cwd).
//! - [`desugar`] — line classification (builtin-at-head vs raw qfs) + lowering each builtin to
//!   closed-core qfs **source statements** (so a builtin produces the same `Plan` as the
//!   equivalent typed statement).
//! - [`session`] — the stateful cwd + [`Session::eval_line`], applying the PREVIEW-by-default /
//!   explicit-COMMIT safety gate uniformly to builtins and raw statements; `cd` is gated by a
//!   pure driver-archetype namespace check.
//! - [`complete`] — the [`Completer`]: builtin names + mount names + path segments via a cheap,
//!   cached, timeout-bounded `ls` (so a slow driver never hangs the prompt).

pub mod complete;
pub mod desugar;
pub mod path;
pub mod session;

pub use complete::{Completer, COMPLETE_TIMEOUT};
pub use desugar::{classify, desugar, Builtin, Desugared, Line};
pub use path::{resolve, VfsPath};
pub use session::{namespace_check, Outcome, Session};
