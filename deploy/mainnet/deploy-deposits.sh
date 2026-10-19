#!/usr/bin/env bash
# Deploys mainnet's Deposits contract on Ethereum: USDC into Railgun's proxy there, screening
# depositors against Chainalysis' sanctions oracle. Reads it back, checks USDC's EIP-712 domain,
# writes deployments/mainnet-deposits.json and, given ETHERSCAN_API_KEY, verifies its source. It
# has no owner: any funded account deploys it. Then enable-deposits.sh turns deposits on on the
# droplet.
#   ETH_RPC_URL=… [ETHERSCAN_API_KEY=…] deploy/mainnet/deploy-deposits.sh <wallet flags>
# <wallet flags> is how cast signs: --interactive (paste a key), --ledger, --account <keystore>, or
# --private-key <key>.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo "name a wallet: --interactive, --ledger, --account <keystore> or --private-key <key>" >&2
    exit 1
fi
: "${ETH_RPC_URL:?set ETH_RPC_URL to an Ethereum mainnet RPC}"
wallet=("$@")

usdc=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48
railgun=0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9
sanctions=0x40C57923924B5c5c5455c48D93317139ADDaC8fb
zecswap=0x4708D7d36992b7e33Ad97a1ab0A31c3ac6C4fe6D
cd "$(dirname "$0")/../.."
manifest=deployments/mainnet-deposits.json
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1
# cast quotes RPC URLs, keys included, in its errors.
hide_urls() { sed -E 's#https?://[^ "]+#<rpc>#g'; }
lower() { tr 'A-F' 'a-f'; }

if [ -e "$manifest" ]; then
    echo "$manifest already exists" >&2
    exit 1
fi
if [ "$(cast chain-id --rpc-url "$ETH_RPC_URL" 2>&1 | hide_urls)" != 1 ]; then
    echo "ETH_RPC_URL is not Ethereum mainnet" >&2
    exit 1
fi
if [ -n "$(git status --porcelain -- contracts/src)" ]; then
    echo "contracts/src has uncommitted changes: deploy committed code" >&2
    exit 1
fi
if [ "$(cast code --rpc-url "$ETH_RPC_URL" "$sanctions" 2>&1 | hide_urls)" = 0x ]; then
    echo "the sanctions oracle $sanctions has no code" >&2
    exit 1
fi
(cd contracts && forge build --quiet)
code=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["bytecode"]["object"])' \
    contracts/out/Deposits.sol/Deposits.json)
args=$(cast abi-encode 'constructor(address,address,address)' "$usdc" "$railgun" "$sanctions")

echo "Deploying Deposits on Ethereum mainnet: USDC into Railgun at $railgun, screened by $sanctions."
read -r -p "Go ahead? [y/N] " answer
[ "$answer" = y ] || exit 1

sent=$(cast send --rpc-url "$ETH_RPC_URL" --json "${wallet[@]}" --create "$code${args#0x}" 2>&1 |
    hide_urls) || {
    echo "$sent" >&2
    exit 1
}
read -r deposits transaction block gas deployer < <(python3 -c 'import json, sys
text = sys.stdin.read()
r, _ = json.JSONDecoder().raw_decode(text[text.index("{"):])
assert r["status"] in ("0x1", 1), "the deployment reverted"
print(r["contractAddress"], r["transactionHash"], int(r["blockNumber"], 16), int(r["gasUsed"], 16), r["from"])' <<<"$sent")
echo "Deposits at $deposits in $transaction."

# cast annotates large numbers, as in "7200 [7.2e3]".
read_back() { cast call --rpc-url "$ETH_RPC_URL" "$1" "$2" 2>&1 | hide_urls | awk '{ print $1 }'; }
check() {
    if [ "$(lower <<<"$2")" != "$(lower <<<"$3")" ]; then
        echo "$1 reads back $2, not $3" >&2
        exit 1
    fi
}
check "TOKEN()" "$(read_back "$deposits" 'TOKEN()(address)')" "$usdc"
check "RAILGUN()" "$(read_back "$deposits" 'RAILGUN()(address)')" "$railgun"
check "SANCTIONS()" "$(read_back "$deposits" 'SANCTIONS()(address)')" "$sanctions"
name=$(cast call --rpc-url "$ETH_RPC_URL" "$usdc" 'name()(string)' 2>&1 | hide_urls | tr -d '"')
version=$(cast call --rpc-url "$ETH_RPC_URL" "$usdc" 'version()(string)' 2>&1 | hide_urls | tr -d '"')
separator=$(cast keccak "$(cast abi-encode 'f(bytes32,bytes32,bytes32,uint256,address)' \
    "$(cast keccak 'EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)')" \
    "$(cast keccak "$name")" "$(cast keccak "$version")" 1 "$usdc")")
check "USDC's DOMAIN_SEPARATOR()" "$(read_back "$usdc" 'DOMAIN_SEPARATOR()(bytes32)')" "$separator"

python3 - "$manifest" "$deposits" "$transaction" "$block" "$gas" "$deployer" "$usdc" "$name" \
    "$version" "$railgun" "$sanctions" "$zecswap" "$(git rev-parse HEAD)" <<'EOF'
import json, sys, time
(path, deposits, transaction, block, gas, deployer, usdc, name, version, railgun, sanctions,
 zecswap, commit) = sys.argv[1:]
manifest = {
    "chainId": 1,
    "deposits": deposits,
    "token": usdc,
    "tokenName": name,
    "tokenVersion": version,
    "tokenDecimals": 6,
    "railgun": railgun,
    "sanctions": sanctions,
    "contract": zecswap,
    "deployment": {"transaction": transaction, "block": int(block), "gasUsed": int(gas)},
    "deployer": deployer,
    "sourceCommit": commit,
    "deployedAt": time.strftime("%Y-%m-%d", time.gmtime()),
    "status": "staged",
}
with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
EOF
echo "Wrote $manifest. Next, on the droplet: enable-deposits.sh <release> $deposits"

verify=(forge verify-contract "$deposits" src/Deposits.sol:Deposits --chain mainnet
    --constructor-args "$args" --watch)
if [ -z "${ETHERSCAN_API_KEY:-}" ]; then
    echo "To verify its source later, from contracts/: ETHERSCAN_API_KEY=… ${verify[*]}"
    exit 0
fi
(cd contracts && "${verify[@]}" --etherscan-api-key "$ETHERSCAN_API_KEY" 2>&1 | hide_urls) ||
    echo "Etherscan verification failed; the deployment stands. Retry: ${verify[*]}" >&2
