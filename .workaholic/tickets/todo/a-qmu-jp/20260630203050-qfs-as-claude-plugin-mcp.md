---
created_at: 2026-06-30T20:30:50+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Infrastructure]
effort: 4h
commit_hash:
category: Added
depends_on: [20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md]
---

# Package qfs as a Claude plugin / MCP that replaces gmail-ftp@ / gdrive-ftp@

Part of EPIC `20260630203000`. gmail-ftp and gdrive-ftp are installed as **Claude plugins**
(`~/.claude/settings.json`: `enabledPlugins: {gmail-ftp@gmail-ftp, gdrive-ftp@gdrive-ftp}`,
`extraKnownMarketplaces` → `qmu/gmail-ftp`, `qmu/gdrive-ftp`). The owner wants the SAME Claude
experience from qfs.

## What qfs already has

- `qfs serve` exposes an **MCP endpoint** (`crate::mcp`) + HTTP + a dashboard (the "three faces, one
  engine"). `qfs skill` prints the AI operating procedure.

## Work

1. Decide the integration shape: (a) an MCP server entry in `~/.claude/settings.json` /
   `.mcp.json` pointing at `qfs serve --mcp` (or the stdio MCP), or (b) a qfs **Claude plugin**
   (skill + commands) published like `qmu/gmail-ftp` so it installs via the marketplace.
2. Wire it so Claude can do the gmail-ftp/gdrive-ftp actions through qfs's MCP tools (describe / run /
   preview / commit over `/mail` and `/drive`).
3. Document install + the settings.json change in the guidance doc (`20260630203040`).

## Key files

- `crates/qfs/src/mcp.rs`, `crate::serve` (the MCP/serve composition). `~/.claude/settings.json`
  (the plugin/MCP registration the owner uses). Reference the gmail-ftp/gdrive-ftp `plugins/` dirs.

## Considerations

- The MCP endpoint is interactively-authenticated; confirm headless/cron behaviour.
- Keep least-privilege: the MCP tool surface should respect qfs policies (PREVIEW-first, irreversible
  gating) so an agent can't `send`/hard-delete without the explicit verb.
