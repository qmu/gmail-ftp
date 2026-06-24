#!/usr/bin/env bash
# Credential-shape grep gate (ticket t40, RFD-0001 §10; reuses the t37 credential-shape idea).
#
# Asserts that NO live-credential shape appears in the documentation, examples, golden fixtures,
# or (when present) the built release artifacts. Docs/examples use PLACEHOLDER handles only; the
# release pipeline ships no secrets. This runs in `release.yml` (and is safe to run locally /
# in CI on every push) — it fails the build if a real-looking token leaks.
#
# It greps for well-known token PREFIXES + obvious secret-assignment shapes. Placeholders like
# `<token>`, `YOUR_TOKEN`, `example`, or a bare `token:` field NAME are fine — those are not live
# secrets. We match the credential VALUE shapes only.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The paths to scan: the authoritative docs, the generated reference, golden fixtures, the
# installer, and the release output dir if it exists. `docs/` lives at the repo root, one level
# above this packages/qfs workspace (ROOT).
SCAN_PATHS=("README.md" "../docs" "install.sh")
[ -d dist ] && SCAN_PATHS+=("dist")
# Golden fixtures across the workspace (checked-in expected outputs an example might embed creds in).
while IFS= read -r d; do SCAN_PATHS+=("$d"); done < <(
  find crates -type d \( -name golden -o -name goldens -o -name fixtures \) 2>/dev/null
)

# Live-credential VALUE shapes (prefixes that only ever front a real secret).
PATTERNS=(
  'ghp_[A-Za-z0-9]{20,}'                 # GitHub personal access token
  'gho_[A-Za-z0-9]{20,}'                 # GitHub OAuth token
  'github_pat_[A-Za-z0-9_]{20,}'         # GitHub fine-grained PAT
  'xox[baprs]-[A-Za-z0-9-]{10,}'         # Slack token
  'sk-[A-Za-z0-9]{20,}'                  # OpenAI-style secret key
  'AKIA[0-9A-Z]{16}'                     # AWS access key id
  'ya29\.[A-Za-z0-9_-]{20,}'             # Google OAuth access token
  '-----BEGIN [A-Z ]*PRIVATE KEY-----'   # an inline private key
  'eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}'  # a JWT value
)

found=0
for pat in "${PATTERNS[@]}"; do
  # -r recursive, -E regex, -I skip binary; report file:line on a hit.
  if hits="$(grep -rIEn "$pat" "${SCAN_PATHS[@]}" 2>/dev/null)"; then
    echo "credential-gate: FOUND a live-credential shape matching /$pat/:" >&2
    echo "$hits" >&2
    found=1
  fi
done

if [ "$found" -ne 0 ]; then
  echo "credential-gate: FAILED — remove the live credential(s) above; use placeholders only (RFD §10)." >&2
  exit 1
fi

echo "credential-gate: OK — no live-credential shape in docs / examples / goldens / artifacts."
