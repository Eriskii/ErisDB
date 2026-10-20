#!/usr/bin/env bash
# Build ErisDB's browser client (clients/web) into apps/shared/erisdb-web/, where Tasks and Lists
# load it, and pin the result in the apps' content security policy.
#
# Needs the wasm32-unknown-unknown Rust target, the wasm-bindgen CLI at the version the crate
# pins (on PATH, at ~/.cargo/bin, or named by WASM_BINDGEN), and a C compiler for wasm32, which
# ring needs: CC_wasm32_unknown_unknown, or clang. Where neither is set up and Nix is, the build
# runs in clients/web/shell.nix, which provides one.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
crate="$root/clients/web"
out="$root/apps/shared/erisdb-web"

bindgen="${WASM_BINDGEN:-$(command -v wasm-bindgen || echo "$HOME/.cargo/bin/wasm-bindgen")}"
[ -x "$bindgen" ] || { echo "build-web: no wasm-bindgen CLI; cargo install wasm-bindgen-cli --version <the crate's>" >&2; exit 1; }
pinned="$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' "$crate/Cargo.toml")"
have="$("$bindgen" --version | awk '{print $2}')"
[ "$have" = "$pinned" ] || { echo "build-web: wasm-bindgen CLI $have does not match the crate's $pinned" >&2; exit 1; }

cd "$crate"
if [ -n "${CC_wasm32_unknown_unknown:-}" ] || ! command -v nix-shell >/dev/null; then
  cargo build --release
else
  nix-shell shell.nix --run 'cargo build --release'
fi

rm -rf "$out"
"$bindgen" --target no-modules --no-modules-global ErisDBWeb --no-typescript \
  --out-dir "$out" target/wasm32-unknown-unknown/release/erisdb_web.wasm
python3 "$root/scripts/update-csp.py"
echo "built $(du -h "$out/erisdb_web_bg.wasm" | cut -f1) of WebAssembly into ${out#"$root"/}"
