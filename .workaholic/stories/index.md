# Branch Stories

* [work-20260703-022500.md](work-20260703-022500.md) - Fix the v0.0.14 piped-stdin passphrase-prompt regression (/dev/tty gate) and port gmail-ftp's paste-back browser consent to `qfs account add google` (SSH-friendly, no listener, OSC 52 copy, state-verified). v0.0.14 → v0.0.15.
* [work-20260702-012808.md](work-20260702-012808.md) - ADR 0008 multi-host account model: mount-bound accounts via `qfs connect`, the `connection` namespace retired for per-layer verbs (init/host/app/account/connect/vault), all docs swept. v0.0.13 → v0.0.14.
* [work-20260628-000332.md](work-20260628-000332.md) - The whole-roadmap night drive: all 40 roadmap tickets t42–t81 implemented in one autonomous run.
* [work-20260625-170038.md](work-20260625-170038.md) - The forward plan for qfs: the architecture-and-direction roadmap (docs/roadmap.md).
* [work-20260629-110121.md](work-20260629-110121.md) - Wire the qfs binary so the docs run true: 9-ticket wire-binary epic (/local, /sql, /git, cloud reads), in-language CREATE CONNECTION + CONNECT defined-paths epics, cookbook-as-Agent-Skills, gmail/gdrive FTP-parity, and the composable array_agg(struct) read pipeline. 106 commits, v0.0.9 → v0.0.12.
* [work-20260624-210651.md](work-20260624-210651.md) - Post-install welcome: install.sh now prints Next steps (test, auth, update, docs). Patch 0.0.2 -> 0.0.3.
* [work-20260624-182641.md](work-20260624-182641.md) - Promote the qfs README to the repo root, add CLAUDE.md, and start the per-PR patch-version rule (0.0.1 -> 0.0.2).
* [work-20260622-230954.md](work-20260622-230954.md) - Rebuild the repo as **qfs**: a single Rust binary exposing every service through one pipe-SQL query language (41-ticket trip), then rename/restructure to a packages/ monorepo, erase gmail-ftp, write user docs + a VitePress/Docker site, ship a working v0.0.1 release, and add the Claude Code plugin.
