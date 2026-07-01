//! `gen-skills` — turn each `docs/cookbook/*.md` article into a Claude Code **Agent Skill**
//! (`plugins/qfs/skills/<name>/SKILL.md`), so qfs's how-to knowledge is loadable on demand by an AI
//! agent, not only readable by a human on the docs site.
//!
//! ## Single source of truth (anti-drift, mirrors `gen-docs`)
//! The human cookbook article is the **authored source**; the `SKILL.md` is **generated** from it —
//! never hand-edited. Each article carries a flat frontmatter block:
//!
//! ```text
//! ---
//! skill_name: qfs-gmail
//! skill_description: Use when a task needs to read or triage Gmail through qfs — …
//! ---
//! # Cookbook: Gmail
//! …the recipes…
//! ```
//!
//! `gen-skills` renders `SKILL.md` = the Claude Code frontmatter (`name` + `description`) followed by
//! the article body verbatim, and (with `--check`) verifies the committed `SKILL.md`, its
//! `.claude/skills/<name>` symlink, and its `marketplace.json` registration are all in sync. This is
//! the exact anti-drift discipline `gen-docs --check` applies to the generated reference docs.
//!
//! Pure `std` (dep-light): no YAML/JSON crate — the frontmatter is a two-key flat block and the
//! marketplace registration is a substring presence check.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The cookbook directory (relative to the repo root) whose articles become skills.
const COOKBOOK_DIR: &str = "docs/cookbook";
/// Where the generated Claude Code skills live (one dir per skill, each holding a `SKILL.md`).
const SKILLS_DIR: &str = "plugins/qfs/skills";
/// The plugin marketplace manifest whose `skills[]` array must list every generated skill.
const MARKETPLACE: &str = ".claude-plugin/marketplace.json";
/// The repo-local skill-loading dir: `.claude/skills/<name>` symlinks into [`SKILLS_DIR`].
const CLAUDE_SKILLS: &str = ".claude/skills";

/// One cookbook article that carries skill frontmatter — the parsed source of a generated skill.
struct SkillSource {
    /// The skill id (`name` in the SKILL.md frontmatter; also the skill dir name).
    name: String,
    /// The "Use when…" trigger the harness matches (`description` in the frontmatter).
    description: String,
    /// The article body (everything after the source frontmatter), copied verbatim into the skill.
    body: String,
}

/// The result of a `gen-skills` run: what was written, and (for `--check`) what drifted.
pub struct Outcome {
    /// Paths written (empty in `--check` mode).
    pub written: Vec<PathBuf>,
    /// Human-readable drift/registration problems (empty ⇒ in sync). `--check` fails if non-empty.
    pub drift: Vec<String>,
}

/// Render (or, with `check`, verify) every cookbook-article skill under `repo_root`.
///
/// # Errors
/// An [`io::Error`] on a filesystem failure (a missing cookbook dir, an unreadable article, a failed
/// write). Content/registration drift is NOT an error — it is reported in [`Outcome::drift`].
pub fn gen_skills(repo_root: &Path, check: bool) -> io::Result<Outcome> {
    let sources = collect_sources(repo_root)?;
    let mut out = Outcome {
        written: Vec::new(),
        drift: Vec::new(),
    };
    let marketplace = fs::read_to_string(repo_root.join(MARKETPLACE)).unwrap_or_default();

    for s in &sources {
        let rendered = render_skill(&s.name, &s.description, &s.body);
        let target = repo_root.join(SKILLS_DIR).join(&s.name).join("SKILL.md");
        let rel = format!("{SKILLS_DIR}/{}/SKILL.md", s.name);

        if check {
            match fs::read_to_string(&target) {
                Ok(existing) if existing == rendered => {}
                Ok(_) => out.drift.push(format!("{rel} (out of date)")),
                Err(_) => out.drift.push(format!("{rel} (missing)")),
            }
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, &rendered)?;
            out.written.push(target);
            ensure_symlink(repo_root, &s.name, &mut out)?;
        }

        // Registration (both modes): the skill must be listed in the marketplace + symlinked so the
        // harness discovers it. In write mode `ensure_symlink` already created the link; here we
        // record any registration gap so `--check` fails and the author wires it up.
        if !marketplace.contains(&format!("./skills/{}", s.name)) {
            out.drift.push(format!(
                "{MARKETPLACE}: skills[] is missing \"./skills/{}\"",
                s.name
            ));
        }
        if check && !symlink_ok(repo_root, &s.name) {
            out.drift
                .push(format!("{CLAUDE_SKILLS}/{} symlink missing/wrong", s.name));
        }
    }
    Ok(out)
}

/// Read every `*.md` under `docs/cookbook/` that carries skill frontmatter, in a deterministic
/// (sorted-by-path) order so the output is byte-stable across runs (the `--check` requirement).
fn collect_sources(repo_root: &Path) -> io::Result<Vec<SkillSource>> {
    let dir = repo_root.join(COOKBOOK_DIR);
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    paths.sort();

    let mut sources = Vec::new();
    for path in paths {
        let content = fs::read_to_string(&path)?;
        if let Some(src) = parse_skill_source(&content) {
            sources.push(src);
        }
    }
    Ok(sources)
}

/// Parse the flat `skill_name` / `skill_description` frontmatter of a cookbook article. Returns
/// `None` if the file has no leading frontmatter or is missing either key (so a plain article is
/// simply skipped, never a hard error).
fn parse_skill_source(content: &str) -> Option<SkillSource> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let front = &rest[..end];
    let body = rest[end + "\n---\n".len()..].trim_start_matches('\n');

    let mut name = None;
    let mut description = None;
    for line in front.lines() {
        if let Some(v) = line.strip_prefix("skill_name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("skill_description:") {
            description = Some(v.trim().to_string());
        }
    }
    Some(SkillSource {
        name: name?,
        description: description?,
        body: body.to_string(),
    })
}

/// Render a `SKILL.md`: the Claude Code frontmatter (`name` + `description`) then the article body
/// verbatim. A single trailing newline keeps the file POSIX-clean and byte-stable.
fn render_skill(name: &str, description: &str, body: &str) -> String {
    let mut out = format!("---\nname: {name}\ndescription: {description}\n---\n\n");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

/// Ensure `.claude/skills/<name>` is a symlink into `plugins/qfs/skills/<name>` (matching the
/// existing `qfs` skill's link), creating it if absent. Records a note in `out.written`.
fn ensure_symlink(repo_root: &Path, name: &str, out: &mut Outcome) -> io::Result<()> {
    let link = repo_root.join(CLAUDE_SKILLS).join(name);
    if symlink_ok(repo_root, name) {
        return Ok(());
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    // A stale/wrong link is replaced.
    let _ = fs::remove_file(&link);
    let target = format!("../../{SKILLS_DIR}/{name}");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link)?;
    out.written.push(link);
    Ok(())
}

/// Whether `.claude/skills/<name>` already points at `plugins/qfs/skills/<name>`.
fn symlink_ok(repo_root: &Path, name: &str) -> bool {
    let link = repo_root.join(CLAUDE_SKILLS).join(name);
    let expected = format!("../../{SKILLS_DIR}/{name}");
    fs::read_link(&link).ok().as_deref() == Some(Path::new(&expected))
}
