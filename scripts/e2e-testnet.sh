#!/usr/bin/env bash
# Live end-to-end run on Base Sepolia and the Zcash testnet.
#
#   scripts/e2e-testnet.sh [scenario ...]
#
# Scenarios: happy no-deposit underpaid silent-maker never-claimed abandoned-claim (default:
# all, concurrently, about 50 minutes). Settings come from the environment or .env.testnet:
#   ZECSWAP_E2E_FUNDER_KEY   Base Sepolia key holding test ETH; deploys, makes, and funds the
#                            other accounts (defaults to MAKER_PRIVATE_KEY)
#   ZECSWAP_E2E_WALLET       zecswap-cli wallet whose account holds testnet ZEC
#                            (defaults to .testnet/user)
#   ZECSWAP_E2E_BASE_RPC     defaults to BASE_SEPOLIA_RPC_URL, then https://sepolia.base.org
#   ZECSWAP_E2E_LIGHTWALLETD defaults to https://testnet.zec.rocks:443
set -euo pipefail
cd "$(dirname "$0")/.."

# .env.testnet only fills in what the environment doesn't already set.
if [[ -f .env.testnet ]]; then
    while IFS='=' read -r key value; do
        if [[ $key =~ ^[A-Z_][A-Z0-9_]*$ && -z ${!key:-} ]]; then
            export "$key=$value"
        fi
    done < .env.testnet
fi
export ZECSWAP_E2E_FUNDER_KEY="${ZECSWAP_E2E_FUNDER_KEY:-${MAKER_PRIVATE_KEY:-}}"
export ZECSWAP_E2E_WALLET="${ZECSWAP_E2E_WALLET:-.testnet/user}"
export ZECSWAP_E2E_BASE_RPC="${ZECSWAP_E2E_BASE_RPC:-${BASE_SEPOLIA_RPC_URL:-https://sepolia.base.org}}"

(cd contracts && FOUNDRY_DISABLE_NIGHTLY_WARNING=1 forge build --silent)
# A sleeping machine misses its reveal deadlines; keep it awake for the whole run.
keep_awake=()
if command -v caffeinate >/dev/null; then
    keep_awake=(caffeinate -is)
fi
exec "${keep_awake[@]}" cargo test -p zecswap-e2e --test live -- "$@"
