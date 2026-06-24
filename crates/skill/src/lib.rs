//! `cfs-skill` — the embedded agent skill (ticket t39, RFD §1/§9).
//!
//! This crate ships the authored **`SKILL.md`** operating procedure — the single uniform loop
//! **DESCRIBE → write a cfs statement → PREVIEW → COMMIT** an AI agent follows to drive every
//! service through `cfs` — and the **golden example corpus** (one worked example per driver),
//! both **embedded at compile time via [`include_str!`]** so they ship *inside* the single `cfs`
//! binary (RFD §9). No `include_dir` crate is pulled (the offline cargo cache lacks it and the
//! disk is tight): each asset is one `include_str!`, listed in [`EXAMPLES`].
//!
//! ## Pure assets — no runtime deps
//! The shipped surface is `&'static str` text and a manifest. The crate has **zero** runtime
//! dependencies; the golden CORPUS (which proves each example parses → evaluates to a `Plan` →
//! its PREVIEW matches a checked-in golden) lives under `tests/` and reuses the **t38 `cfs-test`
//! harness** (`assert_plan` / `golden`) as a dev-dependency — never linked into the binary.
//!
//! ## The thesis: loop uniformity
//! Every example in [`EXAMPLES`] uses the **identical four steps**. That uniformity IS the
//! deliverable: an agent learns one loop, not N SDKs. If an example needed a prose exception, the
//! driver contract (t13) would be under-declaring — the fix belongs there, not here.

#![forbid(unsafe_code)]

/// The authored agent operating procedure, embedded at compile time. An embedding host (the
/// binary, or a skill loader) ships this verbatim so the loop docs travel inside the binary.
pub const SKILL_MD: &str = include_str!("../assets/SKILL.md");

/// One embedded worked example: the driver label + its canonical cfs statement asset. The
/// `source` is the exact statement the golden corpus parses → previews; the `driver` is the
/// service it exercises. All seven share the identical DESCRIBE → statement → PREVIEW → COMMIT
/// structure (the uniformity thesis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Example {
    /// The driver/service this example exercises (e.g. `"mail"`, `"server"`).
    pub driver: &'static str,
    /// The embedded example asset (a `.cfs` file with a comment header + the statement).
    pub source: &'static str,
}

/// The golden example corpus, embedded at compile time (RFD §9) — one worked example per driver,
/// every one using the identical four-step loop. Ships inside the binary alongside [`SKILL_MD`].
///
/// `sql` is a pure read (no COMMIT); the rest produce an effect plan whose PREVIEW the golden
/// corpus pins. The `server` example is a `CREATE TRIGGER` that desugars to a single
/// `/server/triggers` config write (the PREVIEW-as-CI-test pattern, RFD §8).
pub const EXAMPLES: &[Example] = &[
    Example {
        driver: "mail",
        source: include_str!("../assets/examples/mail.cfs"),
    },
    Example {
        driver: "drive",
        source: include_str!("../assets/examples/drive.cfs"),
    },
    Example {
        driver: "github",
        source: include_str!("../assets/examples/github.cfs"),
    },
    Example {
        driver: "slack",
        source: include_str!("../assets/examples/slack.cfs"),
    },
    Example {
        driver: "sql",
        source: include_str!("../assets/examples/sql.cfs"),
    },
    Example {
        driver: "git",
        source: include_str!("../assets/examples/git.cfs"),
    },
    Example {
        driver: "server",
        source: include_str!("../assets/examples/server.cfs"),
    },
];

impl Example {
    /// The statement line of the example asset — the last non-empty, non-comment line. The asset
    /// header is `--` comment lines documenting the DESCRIBE excerpt + the step; the final line is
    /// the executable cfs statement the golden corpus parses.
    #[must_use]
    pub fn statement(&self) -> &str {
        self.source
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with("--"))
            .unwrap_or("")
    }
}

/// Render the embedded skill for `cfs skill`: [`SKILL_MD`] alone, or — when `include_examples` —
/// `SKILL_MD` followed by the [`EXAMPLES`] corpus (each driver's canonical `.cfs` asset under a
/// stable `## Examples` heading). Pure string assembly over the `include_str!` consts — no I/O, no
/// allocation beyond the joined output — so the binary's `cfs skill` arm stays logic-free.
#[must_use]
pub fn render(include_examples: bool) -> String {
    if !include_examples {
        return SKILL_MD.to_string();
    }
    let mut out = String::with_capacity(SKILL_MD.len() + 2048);
    out.push_str(SKILL_MD);
    out.push_str("\n\n## Example corpus (embedded golden examples)\n");
    for ex in EXAMPLES {
        out.push_str(&format!("\n### {}\n```text\n", ex.driver));
        out.push_str(ex.source.trim_end());
        out.push_str("\n```\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_md_documents_the_four_step_loop() {
        // The load-bearing structure an agent reads: the four steps and the archetype vocabulary.
        for needle in [
            "DESCRIBE",
            "PREVIEW",
            "COMMIT",
            "append_log",
            "relational_table",
            "object_graph_workflow",
            "blob_namespace",
            "irreversible",
            "least privilege",
            "UPSERT",
        ] {
            assert!(
                SKILL_MD.contains(needle),
                "SKILL.md is missing the load-bearing element `{needle}`"
            );
        }
    }

    #[test]
    fn corpus_covers_every_required_driver() {
        let drivers: Vec<&str> = EXAMPLES.iter().map(|e| e.driver).collect();
        for required in ["mail", "drive", "github", "slack", "sql", "git", "server"] {
            assert!(
                drivers.contains(&required),
                "the worked-example corpus is missing `{required}`"
            );
        }
        assert_eq!(
            EXAMPLES.len(),
            7,
            "exactly the seven required worked examples"
        );
    }

    #[test]
    fn every_example_extracts_a_statement() {
        for ex in EXAMPLES {
            let stmt = ex.statement();
            assert!(
                !stmt.is_empty(),
                "example `{}` has no executable statement line",
                ex.driver
            );
            // No example smuggles a credential shape into the shipped asset (secrets never appear).
            assert!(!stmt.to_lowercase().contains("token"));
            assert!(!stmt.contains("Bearer "));
        }
    }

    #[test]
    fn render_emits_the_loop_and_optionally_the_corpus() {
        // The plain render is exactly SKILL_MD (what `cfs skill` prints).
        let plain = render(false);
        assert_eq!(plain, SKILL_MD);
        for landmark in ["DESCRIBE", "PREVIEW", "COMMIT"] {
            assert!(plain.contains(landmark), "render(false) lost `{landmark}`");
        }
        // `--examples` appends the corpus under a stable heading, with every driver's statement.
        let full = render(true);
        assert!(
            full.starts_with(SKILL_MD),
            "render(true) keeps SKILL.md first"
        );
        assert!(full.contains("## Example corpus"));
        for ex in EXAMPLES {
            assert!(
                full.contains(ex.statement()),
                "render(true) is missing the `{}` example statement",
                ex.driver
            );
        }
    }
}
