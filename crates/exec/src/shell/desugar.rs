//! Builtin lexing + desugaring (ticket t28, hard part (c)).
//!
//! The shell's filesystem verbs (`ls cd pwd cat cp mv rm`) are **CLI-layer sugar, not grammar
//! keywords** (RFD §3 governance). A line is a builtin iff its **first token** is in the builtin
//! set; everything else parses as a raw closed-core cfs statement. This module owns that
//! distinction and the lowering of each effectful/read builtin into one or more closed-core cfs
//! **source statements** — which the shell then routes through the SAME parse → plan → preview /
//! scan pipeline the one-shot path uses. Desugaring to *source text* (not a hand-built AST) is
//! deliberate: it guarantees a builtin produces byte-for-byte the same `Statement` (hence the
//! same `Plan`) as the equivalent statement typed at the prompt, which is the t28 acceptance.
//!
//! `cd`/`pwd` carry **no plan** — they are pure cwd state changes the [`session`](crate::shell::session)
//! layer performs directly (with a driver capability check for `cd`), so they are modelled here
//! only as their [`Builtin`] tag, never desugared to a statement.

use crate::error::ExecError;
use crate::shell::path::{resolve, VfsPath};

/// The closed builtin set. These names are reserved **only at the line head**; the same word
/// anywhere else (or as a raw statement keyword) is untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `ls [path]` — list a namespace (desugars to a `SELECT` over the listing relation).
    Ls,
    /// `cd <path>` — change the cwd (pure state change; capability-gated by the session).
    Cd,
    /// `pwd` — print the cwd (pure).
    Pwd,
    /// `cat <path>` — read a blob/relation (desugars to a bare `FROM <path>`).
    Cat,
    /// `cp <src> <dst>` — copy (desugars to `UPSERT INTO <dst> FROM <src>`).
    Cp,
    /// `mv <src> <dst>` — move (desugars to copy→delete: `UPSERT … FROM …` then `REMOVE <src>`).
    Mv,
    /// `rm <path>…` — remove (desugars each arg to `REMOVE <path>`).
    Rm,
}

impl Builtin {
    /// Recognise a line-head token as a builtin, else `None` (the line is a raw cfs statement).
    /// Case-sensitive lowercase: the shell verbs are lowercase sugar; the cfs grammar keywords
    /// are conventionally uppercase, so `LS`/`RM` typed at the prompt parse as cfs, never sugar.
    #[must_use]
    pub fn from_head(token: &str) -> Option<Self> {
        match token {
            "ls" => Some(Builtin::Ls),
            "cd" => Some(Builtin::Cd),
            "pwd" => Some(Builtin::Pwd),
            "cat" => Some(Builtin::Cat),
            "cp" => Some(Builtin::Cp),
            "mv" => Some(Builtin::Mv),
            "rm" => Some(Builtin::Rm),
            _ => None,
        }
    }

    /// The builtin name (for completion + help).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Ls => "ls",
            Builtin::Cd => "cd",
            Builtin::Pwd => "pwd",
            Builtin::Cat => "cat",
            Builtin::Cp => "cp",
            Builtin::Mv => "mv",
            Builtin::Rm => "rm",
        }
    }

    /// Whether this builtin produces an **effect** plan (vs a pure read / pure state change).
    /// Drives the PREVIEW/COMMIT gate: only effectful builtins ever need a COMMIT.
    #[must_use]
    pub fn is_effect(self) -> bool {
        matches!(self, Builtin::Cp | Builtin::Mv | Builtin::Rm)
    }

    /// Every builtin name, in display order (for the completer).
    #[must_use]
    pub fn all_names() -> &'static [&'static str] {
        &["ls", "cd", "pwd", "cat", "cp", "mv", "rm"]
    }
}

/// The classification of a typed line: a recognised builtin (with its already-split args) or a
/// raw cfs statement to parse verbatim.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// A builtin invocation: the verb + its whitespace-split argument tokens.
    Builtin {
        /// The recognised builtin.
        verb: Builtin,
        /// The argument tokens (already whitespace-split; quoting is not yet supported).
        args: Vec<String>,
    },
    /// A raw cfs statement (anything whose head is not a builtin).
    Raw(String),
    /// An empty line (whitespace only) — a no-op.
    Empty,
}

/// Classify a typed `line` into a [`Line`] (hard part (c)): split the head token, check it
/// against the builtin set, else treat the whole line as a raw cfs statement.
#[must_use]
pub fn classify(line: &str) -> Line {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Line::Empty;
    }
    let mut tokens = trimmed.split_whitespace();
    let head = tokens.next().unwrap_or("");
    match Builtin::from_head(head) {
        Some(verb) => Line::Builtin {
            verb,
            args: tokens.map(str::to_string).collect(),
        },
        None => Line::Raw(trimmed.to_string()),
    }
}

/// What a builtin lowers to: a sequence of closed-core cfs **source statements** to run in
/// order, plus the rendering intent so the shell knows whether the result is a listing (read)
/// or an effect preview/commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Desugared {
    /// The cfs source statements to run, in order. A single read for `ls`/`cat`; one effect for
    /// `cp`/`rm <one>`; two for cross-mount `mv` (copy then delete); N for `rm a b c`.
    pub statements: Vec<String>,
    /// Whether these are effect statements (need the PREVIEW/COMMIT gate) or a pure read.
    pub is_effect: bool,
}

/// Desugar an effectful/read builtin into closed-core cfs source statements, resolving every
/// path argument against `cwd` first (so a relative `notes/x` becomes the absolute `/local/...`
/// the grammar requires). `cd`/`pwd` are pure state changes handled by the session and are
/// rejected here with a usage error (they have no statement form).
///
/// # Errors
/// [`ExecError`] (kind `usage`) on a wrong argument count or a `cd`/`pwd` passed here.
pub fn desugar(verb: Builtin, args: &[String], cwd: &VfsPath) -> Result<Desugared, ExecError> {
    match verb {
        Builtin::Cd | Builtin::Pwd => Err(ExecError::usage(format!(
            "`{}` is a pure state change with no statement form",
            verb.name()
        ))),
        Builtin::Ls => {
            // `ls` lists the cwd by default, else the (resolved) argument. The listing relation
            // is the driver's `describe` schema; a stable projection keeps the columns legible.
            let target = match args.first() {
                Some(p) => resolve(p, cwd)?,
                None => cwd.clone(),
            };
            Ok(Desugared {
                statements: vec![format!(
                    "FROM {} |> SELECT name, size, is_dir, modified",
                    target.render()
                )],
                is_effect: false,
            })
        }
        Builtin::Cat => {
            let p = arg(args, 0, "cat <path>")?;
            let target = resolve(p, cwd)?;
            Ok(Desugared {
                statements: vec![format!("FROM {}", target.render())],
                is_effect: false,
            })
        }
        Builtin::Cp => {
            let (src, dst) = two_args(args, "cp <src> <dst>")?;
            let src = resolve(src, cwd)?;
            let dst = resolve(dst, cwd)?;
            Ok(Desugared {
                statements: vec![copy_stmt(&src, &dst)],
                is_effect: true,
            })
        }
        Builtin::Mv => {
            // Cross-source `mv` lowers to copy→verify→delete (RFD §6 recovery): the copy is an
            // ordinary `UPSERT … FROM …` (the local driver's applier does the streaming
            // copy+verify), then the source is removed. Two statements, both dry-runnable; the
            // shell previews BOTH affected counts and applies nothing until COMMIT.
            let (src, dst) = two_args(args, "mv <src> <dst>")?;
            let src = resolve(src, cwd)?;
            let dst = resolve(dst, cwd)?;
            Ok(Desugared {
                statements: vec![copy_stmt(&src, &dst), remove_stmt(&src)],
                is_effect: true,
            })
        }
        Builtin::Rm => {
            if args.is_empty() {
                return Err(ExecError::usage("rm <path>… requires at least one path"));
            }
            // Each arg is its own `REMOVE <target>` (a target may be a glob the driver expands to
            // a set). The shell previews the union of affected counts; COMMIT applies all.
            let statements = args
                .iter()
                .map(|a| resolve(a, cwd).map(|p| remove_stmt(&p)))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Desugared {
                statements,
                is_effect: true,
            })
        }
    }
}

/// `UPSERT INTO <dst> FROM <src>` — the copy desugar shared by `cp` and `mv`'s first leg.
/// UPSERT (not INSERT) is the **retry-safe, idempotent** universal write the blob/namespace
/// archetype declares (RFD §6): re-running a copy after a partial failure converges rather than
/// erroring on an existing destination, which is exactly the recovery shape `mv`'s
/// copy→verify→delete depends on. (INSERT is reserved for append-only/relational targets.)
fn copy_stmt(src: &VfsPath, dst: &VfsPath) -> String {
    format!("UPSERT INTO {} FROM {}", dst.render(), src.render())
}

/// `REMOVE <target>` — the delete desugar shared by `rm` and `mv`'s second leg.
fn remove_stmt(target: &VfsPath) -> String {
    format!("REMOVE {}", target.render())
}

/// Fetch the `n`-th argument or a usage error naming the expected form.
fn arg<'a>(args: &'a [String], n: usize, usage: &str) -> Result<&'a String, ExecError> {
    args.get(n)
        .ok_or_else(|| ExecError::usage(format!("usage: {usage}")))
}

/// Require exactly two arguments (`<src> <dst>`).
fn two_args<'a>(args: &'a [String], usage: &str) -> Result<(&'a String, &'a String), ExecError> {
    if args.len() != 2 {
        return Err(ExecError::usage(format!("usage: {usage}")));
    }
    Ok((&args[0], &args[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> VfsPath {
        VfsPath::new("local", vec!["docs".into()])
    }

    #[test]
    fn line_head_only_recognises_builtins() {
        assert_eq!(
            classify("ls notes"),
            Line::Builtin {
                verb: Builtin::Ls,
                args: vec!["notes".into()]
            }
        );
        // A cfs keyword at the head is NOT a builtin — it parses as a raw statement.
        assert_eq!(
            classify("FROM /local |> LIMIT 1"),
            Line::Raw("FROM /local |> LIMIT 1".into())
        );
        // Uppercase `LS` is not the lowercase sugar.
        assert!(matches!(classify("LS x"), Line::Raw(_)));
        assert_eq!(classify("   "), Line::Empty);
    }

    #[test]
    fn ls_desugars_to_select_over_cwd() {
        let d = desugar(Builtin::Ls, &[], &cwd()).unwrap();
        assert_eq!(
            d.statements,
            vec!["FROM /local/docs |> SELECT name, size, is_dir, modified"]
        );
        assert!(!d.is_effect);
    }

    #[test]
    fn ls_with_relative_arg_resolves_against_cwd() {
        let d = desugar(Builtin::Ls, &["sub".into()], &cwd()).unwrap();
        assert_eq!(
            d.statements[0],
            "FROM /local/docs/sub |> SELECT name, size, is_dir, modified"
        );
    }

    #[test]
    fn cat_desugars_to_bare_from() {
        let d = desugar(Builtin::Cat, &["a.md".into()], &cwd()).unwrap();
        assert_eq!(d.statements, vec!["FROM /local/docs/a.md"]);
        assert!(!d.is_effect);
    }

    #[test]
    fn cp_desugars_to_insert_from() {
        let d = desugar(Builtin::Cp, &["a.md".into(), "b.md".into()], &cwd()).unwrap();
        assert_eq!(
            d.statements,
            vec!["UPSERT INTO /local/docs/b.md FROM /local/docs/a.md"]
        );
        assert!(d.is_effect);
    }

    #[test]
    fn cp_cross_mount_keeps_each_driver() {
        // src under cwd's driver, dst under another driver, resolved independently — no `cd`.
        let d = desugar(
            Builtin::Cp,
            &["report.md".into(), "/mail/drafts/report".into()],
            &cwd(),
        )
        .unwrap();
        assert_eq!(
            d.statements,
            vec!["UPSERT INTO /mail/drafts/report FROM /local/docs/report.md"]
        );
    }

    #[test]
    fn mv_is_copy_then_delete() {
        let d = desugar(Builtin::Mv, &["a.md".into(), "/mail/x".into()], &cwd()).unwrap();
        assert_eq!(
            d.statements,
            vec![
                "UPSERT INTO /mail/x FROM /local/docs/a.md",
                "REMOVE /local/docs/a.md",
            ]
        );
        assert!(d.is_effect);
    }

    #[test]
    fn rm_set_is_one_remove_per_arg() {
        let d = desugar(Builtin::Rm, &["a.md".into(), "sub/b.md".into()], &cwd()).unwrap();
        assert_eq!(
            d.statements,
            vec!["REMOVE /local/docs/a.md", "REMOVE /local/docs/sub/b.md"]
        );
        assert!(d.is_effect);
    }

    #[test]
    fn rm_requires_an_arg() {
        let e = desugar(Builtin::Rm, &[], &cwd()).unwrap_err();
        assert_eq!(e.kind.as_str(), "usage");
    }

    #[test]
    fn cd_pwd_have_no_statement_form() {
        assert!(desugar(Builtin::Cd, &["x".into()], &cwd()).is_err());
        assert!(desugar(Builtin::Pwd, &[], &cwd()).is_err());
    }

    #[test]
    fn cp_arity_is_enforced() {
        assert!(desugar(Builtin::Cp, &["only-one".into()], &cwd()).is_err());
    }
}
