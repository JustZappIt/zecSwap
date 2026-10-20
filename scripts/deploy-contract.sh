#!/usr/bin/env bash
# Deploys ZecSwap on Ethereum mainnet paying into Railgun's proxy there, reads its settings back,
# writes deployments/<name>.json and, given ETHERSCAN_API_KEY, verifies its source on Etherscan.
# The contract has no owner: the deploying account keeps no power over it, so any funded account
# will do.
#   ETH_RPC_URL=… [ETHERSCAN_API_KEY=…] scripts/deploy-contract.sh <name> <min window> <late window> <wallet>
# The windows are in seconds: the least an open leaves before t0 and from t0 to t1, and how long
# from t1 only the maker may refund a forward swap. <wallet> is how forge signs: --interactive
# (paste a key), --ledger, or --account <keystore>.
#   scripts/deploy-contract.sh mainnet-windows 3600 86400 --interactive
set -euo pipefail

usage="usage: $0 <name> <min window seconds> <late window seconds> <wallet flags>"
name=${1:?$usage}
min=${2:?$usage}
late=${3:?$usage}
shift 3
if [ "$#" -eq 0 ]; then
    echo "name a wallet: --interactive, --ledger or --account <keystore>" >&2
    exit 1
fi
: "${ETH_RPC_URL:?set ETH_RPC_URL to an Ethereum mainnet RPC}"

railgun=0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9
usdc=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
cd "$(dirname "$0")/.."
manifest=deployments/$name.json
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1
# anvil and forge quote RPC URLs, keys included, in their errors.
hide_urls() { sed -E 's#https?://[^ "]+#<rpc>#g'; }

if [ -e "$manifest" ]; then
    echo "$manifest already exists" >&2
    exit 1
fi
if [ "$(cast chain-id --rpc-url "$ETH_RPC_URL" 2>&1 | hide_urls)" != 1 ]; then
    echo "ETH_RPC_URL is not Ethereum mainnet" >&2
    exit 1
fi
if ! [[ "$min" =~ ^[0-9]+$ ]] || [ "$min" -lt 1800 ] || [ "$min" -gt 14400 ]; then
    echo "a minimum window of $min seconds is outside 1800 to 14400" >&2
    exit 1
fi
if ! [[ "$late" =~ ^[0-9]+$ ]] || [ "$late" -lt "$min" ] || [ "$late" -gt 604800 ]; then
    echo "a late window of $late seconds is outside $min to 604800" >&2
    exit 1
fi
if [ -n "$(git status --porcelain -- contracts/src)" ]; then
    echo "contracts/src has uncommitted changes: deploy committed code" >&2
    exit 1
fi

(cd contracts && forge build --quiet)
echo "Deploying ZecSwap on Ethereum mainnet: ${min} s minimum windows, a ${late} s late window,"
echo "paying into Railgun at $railgun."
read -r -p "Go ahead? [y/N] " answer
[ "$answer" = y ] || exit 1

created=$(cd contracts && forge create src/ZecSwap.sol:ZecSwap --rpc-url "$ETH_RPC_URL" \
    --broadcast --json "$@" --constructor-args "$min" "$late" "$railgun" 2>&1 | hide_urls) || {
    echo "$created" >&2
    exit 1
}
read -r contract deployer transaction < <(python3 -c 'import json, sys
text = sys.stdin.read()
out, _ = json.JSONDecoder().raw_decode(text[text.index("{"):])
print(out["deployedTo"], out["deployer"], out["transactionHash"])' <<<"$created" 2>/dev/null) || true
if [ -z "${transaction:-}" ]; then
    echo "forge reported no deployment:" >&2
    echo "$created" >&2
    exit 1
fi
echo "Deployed at $contract in $transaction."

# cast annotates large numbers, as in "7200 [7.2e3]".
read_back() { cast call --rpc-url "$ETH_RPC_URL" "$contract" "$1" 2>&1 | hide_urls | awk '{ print $1 }'; }
[ "$(read_back 'MIN_WINDOW()(uint256)')" = "$min" ] || { echo "MIN_WINDOW reads back wrong" >&2; exit 1; }
[ "$(read_back 'LATE_WINDOW()(uint256)')" = "$late" ] || { echo "LATE_WINDOW reads back wrong" >&2; exit 1; }
[ "$(read_back 'RAILGUN()(address)' | tr 'A-F' 'a-f')" = "$(tr 'A-F' 'a-f' <<<"$railgun")" ] ||
    { echo "RAILGUN reads back wrong" >&2; exit 1; }
receipt=$(cast receipt --rpc-url "$ETH_RPC_URL" --json "$transaction" 2>&1 | hide_urls)

python3 - "$manifest" "$contract" "$deployer" "$transaction" "$min" "$late" "$railgun" "$usdc" \
    "$(git rev-parse HEAD)" "$receipt" <<'EOF'
import json, sys, time
path, contract, deployer, transaction, min_window, late_window, railgun, usdc, commit, receipt = sys.argv[1:]
receipt = json.loads(receipt)
manifest = {
    "chainId": 1,
    "contract": contract,
    "token": usdc,
    "railgun": railgun,
    "minWindowSeconds": int(min_window),
    "lateWindowSeconds": int(late_window),
    "deploymentTransaction": transaction,
    "deploymentBlock": int(receipt["blockNumber"], 16),
    "deploymentGasUsed": int(receipt["gasUsed"], 16),
    "zcashNetwork": "mainnet",
    "deployer": deployer,
    "sourceCommit": commit,
    "deployedAt": time.strftime("%Y-%m-%d", time.gmtime()),
    "status": "beta",
}
with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
EOF
echo "Wrote $manifest."

verify=(forge verify-contract "$contract" src/ZecSwap.sol:ZecSwap --chain mainnet
    --constructor-args "$(cast abi-encode 'constructor(uint256,uint256,address)' "$min" "$late" "$railgun")" --watch)
if [ -z "${ETHERSCAN_API_KEY:-}" ]; then
    echo "To verify its source later, from contracts/: ETHERSCAN_API_KEY=… ${verify[*]}"
    exit 0
fi
(cd contracts && "${verify[@]}" --etherscan-api-key "$ETHERSCAN_API_KEY" 2>&1 | hide_urls) ||
    echo "Etherscan verification failed; the deployment stands. Retry: ${verify[*]}" >&2
