#!/usr/bin/env bash
# Deploys the hosted testnet's windows contract on Ethereum Sepolia (docs/reverse-flow.md,
# Windows): ZecSwap with a 30-minute minimum window and a 1-hour late window, paying into Railgun's
# Sepolia proxy. Sepolia's repricing breaks `forge script`, so it goes by `cast send --create` at
# the node's own gas estimate and a tenth more. Reads its settings back, checks it has no lock, and
# writes deployments/sepolia-windows.json. The contract has no owner: any funded account deploys
# it. Then switch-windows.sh moves the droplet onto it.
#   ETH_RPC_URL=… deploy/testnet/deploy-windows.sh <wallet flags>
# <wallet flags> is how cast signs: --interactive (paste a key), --ledger, --account <keystore>, or
# --private-key <key>.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo "name a wallet: --interactive, --ledger, --account <keystore> or --private-key <key>" >&2
    exit 1
fi
: "${ETH_RPC_URL:?set ETH_RPC_URL to an Ethereum Sepolia RPC}"
wallet=("$@")

min_window=1800
late_window=3600
railgun=0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea
token=0x64c4a5cb09fe0b706d4cf24dc0a45fff275d727c
lock_contract=0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c
cd "$(dirname "$0")/../.."
manifest=deployments/sepolia-windows.json
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1
# cast quotes RPC URLs, keys included, in its errors.
hide_urls() { sed -E 's#https?://[^ "]+#<rpc>#g'; }
lower() { tr 'A-F' 'a-f'; }

if [ -e "$manifest" ]; then
    echo "$manifest already exists" >&2
    exit 1
fi
if [ "$(cast chain-id --rpc-url "$ETH_RPC_URL" 2>&1 | hide_urls)" != 11155111 ]; then
    echo "ETH_RPC_URL is not Ethereum Sepolia" >&2
    exit 1
fi
if [ -n "$(git status --porcelain -- contracts/src)" ]; then
    echo "contracts/src has uncommitted changes: deploy committed code" >&2
    exit 1
fi
(cd contracts && forge build --quiet)
args=$(cast abi-encode 'constructor(uint256,uint256,address)' "$min_window" "$late_window" "$railgun")
code=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["bytecode"]["object"])' \
    contracts/out/ZecSwap.sol/ZecSwap.json)${args#0x}
estimate=$(cast estimate --rpc-url "$ETH_RPC_URL" --create "$code" 2>&1 | hide_urls)
[[ $estimate =~ ^[0-9]+$ ]] || {
    echo "the node gave no estimate: $estimate" >&2
    exit 1
}

echo "Deploying ZecSwap on Ethereum Sepolia: ${min_window} s minimum windows, a ${late_window} s late"
echo "window, paying into Railgun at $railgun; the node estimates $estimate gas."
read -r -p "Go ahead? [y/N] " answer
[ "$answer" = y ] || exit 1

sent=$(cast send --rpc-url "$ETH_RPC_URL" --json "${wallet[@]}" --gas-limit $((estimate * 11 / 10)) \
    --create "$code" 2>&1 | hide_urls) || {
    echo "$sent" >&2
    exit 1
}
read -r contract transaction block gas deployer < <(python3 -c 'import json, sys
text = sys.stdin.read()
r, _ = json.JSONDecoder().raw_decode(text[text.index("{"):])
assert r["status"] in ("0x1", 1), "the deployment reverted"
print(r["contractAddress"], r["transactionHash"], int(r["blockNumber"], 16), int(r["gasUsed"], 16), r["from"])' <<<"$sent")
echo "Deployed at $contract in $transaction."

# cast annotates large numbers, as in "7200 [7.2e3]".
read_back() { cast call --rpc-url "$ETH_RPC_URL" "$contract" "$1" 2>&1 | hide_urls | awk '{ print $1 }'; }
check() {
    if [ "$(lower <<<"$2")" != "$(lower <<<"$3")" ]; then
        echo "$1 reads back $2, not $3" >&2
        exit 1
    fi
}
check "MIN_WINDOW()" "$(read_back 'MIN_WINDOW()(uint256)')" "$min_window"
check "LATE_WINDOW()" "$(read_back 'LATE_WINDOW()(uint256)')" "$late_window"
check "RAILGUN()" "$(read_back 'RAILGUN()(address)')" "$railgun"
if cast call --rpc-url "$ETH_RPC_URL" "$contract" 'LOCK_DURATION()(uint256)' >/dev/null 2>&1; then
    echo "the contract answers LOCK_DURATION(): it is the lock contract" >&2
    exit 1
fi

python3 - "$manifest" "$contract" "$token" "$railgun" "$min_window" "$late_window" "$transaction" \
    "$block" "$gas" "$deployer" "$(git rev-parse HEAD)" "$lock_contract" <<'EOF'
import json, sys, time
(path, contract, token, railgun, min_window, late_window, transaction, block, gas, deployer, commit,
 replaces) = sys.argv[1:]
manifest = {
    "chainId": 11155111,
    "contract": contract,
    "token": token,
    "railgun": railgun.lower(),
    "minWindowSeconds": int(min_window),
    "lateWindowSeconds": int(late_window),
    "deploymentTransaction": transaction,
    "deploymentBlock": int(block),
    "deploymentGasUsed": int(gas),
    "zcashNetwork": "testnet",
    "deployer": deployer,
    "sourceCommit": commit,
    "deployedAt": time.strftime("%Y-%m-%d", time.gmtime()),
    "replaces": replaces,
    "status": "staged",
}
with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
EOF
echo "Wrote $manifest. Next, on the droplet: switch-windows.sh <release> $contract $min_window $late_window"
