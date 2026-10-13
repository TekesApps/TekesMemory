#!/bin/sh
set -eu
# Explicitly opt in with a clean Kernel checkout; all build artifacts are temporary.
: "${TEKES_KERNEL_ROOT:?absolute Kernel checkout required}"
: "${TEKES_MEMORY_BIN:?absolute Memory binary required}"
memory_repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
harness_root=$(mktemp -d /tmp/tekesmemory-kernel-mcp.XXXXXX)
trap 'rm -rf "$harness_root"' EXIT HUP INT TERM
python3 - "$harness_root" "$memory_repo" "$TEKES_KERNEL_ROOT" <<'PY'
import json, pathlib, sys
root, memory, kernel = map(pathlib.Path, sys.argv[1:])
manifest = '[package]\nname="tekesmemory-kernel-mcp-check"\nversion="0.0.0"\nedition="2024"\n[workspace]\n[[bin]]\nname="interop"\npath=' + json.dumps(str(memory/'tests/support/kernel_mcp_interop.rs')) + '\n[dependencies]\n'
for name, path in [('mcp',kernel/'crates/mcp'),('schema',kernel/'crates/schema'),('tekes-memory',memory)]:
    manifest += name + '={path=' + json.dumps(str(path)) + '}\n'
manifest += 'serde_json="1"\nlibc="0.2"\ntempfile="3"\ntokio={version="1",features=["macros","rt-multi-thread"]}\n'
(root/'Cargo.toml').write_text(manifest)
PY
CARGO_TARGET_DIR="${TEKES_MEMORY_INTEROP_TARGET:-/tmp/tekesmemory-rmcp-interop-target}" cargo run --manifest-path "$harness_root/Cargo.toml" --bin interop
evidence_dir="${TEKES_MEMORY_INTEROP_EVIDENCE_DIR:-/tmp/tekesmemory-rmcp-kernel-mcp-evidence}"
mkdir -p "$evidence_dir"
cp "$harness_root/Cargo.lock" "$evidence_dir/Cargo.lock"
git -C "$TEKES_KERNEL_ROOT" rev-parse HEAD > "$evidence_dir/kernel-head.txt"
git -C "$TEKES_KERNEL_ROOT" status --short > "$evidence_dir/kernel-status.txt"
git -C "$memory_repo" rev-parse HEAD > "$evidence_dir/memory-head.txt"
git -C "$memory_repo" status --short > "$evidence_dir/memory-status.txt"
shasum -a 256 "$TEKES_MEMORY_BIN" > "$evidence_dir/memory-binary-sha256.txt"
cargo tree --manifest-path "$harness_root/Cargo.toml" --depth 1 > "$evidence_dir/dependencies.txt"
