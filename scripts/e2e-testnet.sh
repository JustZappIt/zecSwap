#!/usr/bin/env bash
# Live end-to-end run on an EVM testnet and the Zcash testnet.
#
#   scripts/e2e-testnet.sh [--railgun [--fork]] [scenario ...]
#
# Without flags it runs on Base Sepolia. --railgun runs on Ethereum Sepolia, where Railgun is,
# and adds the scenarios that pay into it; with --fork, on a local anvil fork of it instead,
# which needs no Sepolia ETH (Railgun's screening doesn't run there).
#
# Scenarios: happy no-deposit underpaid silent-maker never-claimed abandoned-claim, and with
# --railgun railgun-happy railgun-resume railgun-no-deposit railgun-send (default: all,
# concurrently). Deposits count after 2 confirmations, which with deadlines to match takes a run
# about 25 minutes; set ZECSWAP_E2E_CONFIRMATIONS=10, the wallets' default, for about 50.
# railgun-send proves with Railgun's wallet SDK: run `npm ci` in crates/zecswap-railgun/engine
# first. Settings come from the environment or .env.testnet:
#   ZECSWAP_E2E_FUNDER_KEY   key holding test ETH on the chain; deploys, makes, and funds the
#                            other accounts (defaults to MAKER_PRIVATE_KEY)
#   ZECSWAP_E2E_WALLET       zecswap-cli wallet whose account holds testnet ZEC
#                            (defaults to .testnet/user)
#   ZECSWAP_E2E_EVM_RPC      defaults to ZECSWAP_E2E_BASE_RPC, then BASE_SEPOLIA_RPC_URL, then
#                            https://sepolia.base.org; with --railgun, ETH_SEPOLIA_RPC_URL
#   ZECSWAP_E2E_RAILGUN_RPC  where Railgun's SDK reads Sepolia, which needs wide log ranges:
#                            defaults to the fork with --fork, else a public Sepolia node
#   ZECSWAP_E2E_LIGHTWALLETD defaults to https://testnet.zec.rocks:443
#   ZECSWAP_E2E_CONFIRMATIONS defaults to 2
set -euo pipefail
cd "$(dirname "$0")/.."

RAILGUN_SEPOLIA=0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea
# anvil's first default account, funded on every local chain.
ANVIL_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

railgun=false
fork=false
while [[ ${1:-} == --* ]]; do
    case $1 in
        --railgun) railgun=true ;;
        --fork) fork=true ;;
        *) echo "unknown flag $1" >&2; exit 2 ;;
    esac
    shift
done
if $fork && ! $railgun; then
    echo "--fork forks Ethereum Sepolia for --railgun" >&2
    exit 2
fi

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
export ZECSWAP_E2E_CONFIRMATIONS="${ZECSWAP_E2E_CONFIRMATIONS:-2}"
if $railgun; then
    : "${ETH_SEPOLIA_RPC_URL:?set ETH_SEPOLIA_RPC_URL for --railgun}"
    export ZECSWAP_E2E_EVM_RPC="${ZECSWAP_E2E_EVM_RPC:-$ETH_SEPOLIA_RPC_URL}"
    export ZECSWAP_E2E_RAILGUN="$RAILGUN_SEPOLIA"
    if ! $fork; then
        export ZECSWAP_E2E_RAILGUN_RPC="${ZECSWAP_E2E_RAILGUN_RPC:-https://ethereum-sepolia-rpc.publicnode.com}"
    fi
else
    export ZECSWAP_E2E_EVM_RPC="${ZECSWAP_E2E_EVM_RPC:-${ZECSWAP_E2E_BASE_RPC:-${BASE_SEPOLIA_RPC_URL:-https://sepolia.base.org}}}"
fi

(cd contracts && FOUNDRY_DISABLE_NIGHTLY_WARNING=1 forge build --silent)

if $fork; then
    port=$((20000 + RANDOM % 10000))
    # The maker's clock is chain time, so the fork must mine on an interval.
    # Railgun's contracts read many slots the fork fetches upstream: give each fetch time.
    anvil --fork-url "$ZECSWAP_E2E_EVM_RPC" --block-time 2 --port "$port" --timeout 120000 \
        --retries 10 --silent &
    anvil_pid=$!
    trap 'kill $anvil_pid 2>/dev/null' EXIT
    export ZECSWAP_E2E_EVM_RPC="http://127.0.0.1:$port"
    export ZECSWAP_E2E_RAILGUN_RPC="$ZECSWAP_E2E_EVM_RPC"
    export ZECSWAP_E2E_FUNDER_KEY="$ANVIL_KEY"
    until cast block-number --rpc-url "$ZECSWAP_E2E_EVM_RPC" >/dev/null 2>&1; do sleep 1; done
fi

# A sleeping machine misses its reveal deadlines; keep it awake for the whole run.
keep_awake=()
if command -v caffeinate >/dev/null; then
    keep_awake=(caffeinate -is)
fi
# Not exec'd, so the trap can stop the fork afterwards.
"${keep_awake[@]}" cargo test -p zecswap-e2e --test live -- "$@"
