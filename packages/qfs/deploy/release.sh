#!/usr/bin/env bash
# t36 release-artifact build (RFD-0001 §9; docs/adr/0005-deployment-hosts.md).
#
# Emits the two-target deployment artifacts into ./dist:
#   * static musl `qfs` daemon binaries for BOTH arches (x86_64 + aarch64-unknown-linux-musl),
#     built with `--features host-daemon` and the size-tuned `[profile.release]`;
#   * the size-optimized `qfs.wasm` Workers module (the `wasm-release` profile: opt-level="z",
#     fat LTO, strip, panic=abort) — the wasm-clean qfs-host core today (the `worker`-backed
#     WorkersHost is PARKED per ADR-0005);
#   * a `wrangler.toml` template (binding NAMES only, never a token — RFD §10);
#   * a `SHA256SUMS` over every emitted artifact.
#
# IMPORTANT (the t01/A2 + t36 constraint): the static musl cross-link is CI-ONLY. The trip host
# has no x86_64 musl linker, so this script is RUN IN THE CI release job (which installs
# `musl-tools` + the aarch64 cross-linker). Locally, the NATIVE release build
# (`cargo build --release --features host-daemon`) is what is verified — do not expect the musl
# link to succeed without the musl toolchain installed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
DIST="$ROOT/dist"
rm -rf "$DIST"
mkdir -p "$DIST"

MUSL_TARGETS=("x86_64-unknown-linux-musl" "aarch64-unknown-linux-musl")

echo "==> Building static musl daemon binaries (--features host-daemon, [profile.release])"
for target in "${MUSL_TARGETS[@]}"; do
  echo "    target: $target"
  cargo build --release --target "$target" --features host-daemon -p qfs
  cp "target/$target/release/qfs" "$DIST/qfs-$target"
done

echo "==> Building the size-optimized qfs.wasm Workers module (--profile wasm-release)"
# The wasm-clean qfs-host core (the `worker`-backed WorkersHost is parked — ADR-0005). When the
# `worker` crate lands, this builds the `worker` cdylib instead; the artifact name is unchanged.
cargo build --profile wasm-release --target wasm32-unknown-unknown -p qfs-host
WASM_SRC="target/wasm32-unknown-unknown/wasm-release/qfs_host.wasm"
if [[ -f "$WASM_SRC" ]]; then
  cp "$WASM_SRC" "$DIST/qfs.wasm"
else
  # qfs-host is a lib crate; its wasm artifact is the rlib's cdylib when host-workers ships the
  # cdylib crate-type. Until then, record the parked state explicitly rather than faking a binary.
  echo "    NOTE: qfs.wasm cdylib is parked (needs the host-workers cdylib crate-type + worker)." \
    > "$DIST/qfs.wasm.PARKED"
fi

echo "==> Emitting the wrangler.toml template (binding NAMES only)"
# A deployment regenerates this per-config; the checked-in golden is the canonical SHAPE the
# generator emits (Cron / Queue / DO / d1·r2·kv bindings by name). Shipped as the template.
cp "$ROOT/crates/host/fixtures/wrangler.golden.toml" "$DIST/wrangler.toml.template"

echo "==> Writing SHA256SUMS"
( cd "$DIST" && sha256sum ./* > SHA256SUMS )

echo "==> Release artifacts in $DIST:"
ls -la "$DIST"
