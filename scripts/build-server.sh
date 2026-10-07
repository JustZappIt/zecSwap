#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
build_tools="$PWD/.testnet/build-tools"
if [[ ! -x "$build_tools/bin/python" ]]; then
  uv venv "$build_tools"
fi
uv pip install --python "$build_tools/bin/python" cargo-zigbuild==0.23.4 ziglang==0.13.0
rustup target add x86_64-unknown-linux-gnu
export PATH="$build_tools/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
export CARGO_PROFILE_RELEASE_STRIP=symbols
cargo zigbuild --locked --release --target x86_64-unknown-linux-gnu.2.39 \
  -p zecswap-maker -p zecswap-relayer -p zecswap-issuer
