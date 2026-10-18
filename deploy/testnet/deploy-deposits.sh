#!/usr/bin/env bash
# Deploys the hosted testnet's deposits on Ethereum Sepolia: a new tUSD, the test token that has
# EIP-3009 as USDC does (contracts/test/utils/TestToken.sol), and the Deposits contract taking it
# into Railgun's Sepolia proxy, screening nobody. Sepolia's repricing breaks `forge script`, so each
# goes by `cast send --create` at the node's own gas estimate. Reads both back, checks the token's
# EIP-712 domain, and writes deployments/sepolia-deposits.json. Neither contract has an owner: any
# funded account deploys them. Then switch-token.sh moves the droplet onto them.
#   ETH_RPC_URL=… deploy/testnet/deploy-deposits.sh <wallet flags>
# <wallet flags> is how cast signs: --interactive (paste a key), --ledger, --account <keystore>, or
# --private-key <key>.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo "name a wallet: --interactive, --ledger, --account <keystore> or --private-key <key>" >&2
    exit 1
fi
: "${ETH_RPC_URL:?set ETH_RPC_URL to an Ethereum Sepolia RPC}"
wallet=("$@")

railgun=0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea
zecswap=0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c
nobody=0x0000000000000000000000000000000000000000
cd "$(dirname "$0")/../.."
manifest=deployments/sepolia-deposits.json
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
if [ -n "$(git status --porcelain -- contracts/src contracts/test/utils/TestToken.sol)" ]; then
    echo "the contracts have uncommitted changes: deploy committed code" >&2
    exit 1
fi
(cd contracts && forge build --quiet)
code() {
    python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["bytecode"]["object"])' \
        "contracts/out/$1.sol/$1.json"
}

echo "Deploying a new tUSD and Deposits on Ethereum Sepolia, shielding into Railgun at $railgun."
read -r -p "Go ahead? [y/N] " answer
[ "$answer" = y ] || exit 1

# Creates `$1`'s code with `$2` appended: its address, transaction, block, gas and deployer.
create() {
    local sent
    sent=$(cast send --rpc-url "$ETH_RPC_URL" --json "${wallet[@]}" --create "$1$2" 2>&1 |
        hide_urls) || {
        echo "$sent" >&2
        return 1
    }
    python3 -c 'import json, sys
text = sys.stdin.read()
r, _ = json.JSONDecoder().raw_decode(text[text.index("{"):])
assert r["status"] in ("0x1", 1), "the deployment reverted"
print(r["contractAddress"], r["transactionHash"], int(r["blockNumber"], 16), int(r["gasUsed"], 16), r["from"])' <<<"$sent"
}

read -r token token_tx token_block token_gas deployer < <(create "$(code TestToken)" "")
echo "tUSD at $token in $token_tx."
args=$(cast abi-encode 'constructor(address,address,address)' "$token" "$railgun" "$nobody")
read -r deposits deposits_tx deposits_block deposits_gas _ < <(create "$(code Deposits)" "${args#0x}")
echo "Deposits at $deposits in $deposits_tx."

# cast annotates large numbers, as in "7200 [7.2e3]".
read_back() { cast call --rpc-url "$ETH_RPC_URL" "$1" "$2" 2>&1 | hide_urls | awk '{ print $1 }'; }
check() {
    if [ "$(lower <<<"$2")" != "$(lower <<<"$3")" ]; then
        echo "$1 reads back $2, not $3" >&2
        exit 1
    fi
}
check "TOKEN()" "$(read_back "$deposits" 'TOKEN()(address)')" "$token"
check "RAILGUN()" "$(read_back "$deposits" 'RAILGUN()(address)')" "$railgun"
check "SANCTIONS()" "$(read_back "$deposits" 'SANCTIONS()(address)')" "$nobody"
check "symbol()" "$(read_back "$token" 'symbol()(string)')" '"tUSD"'
check "decimals()" "$(read_back "$token" 'decimals()(uint8)')" 6
name=$(cast call --rpc-url "$ETH_RPC_URL" "$token" 'name()(string)' 2>&1 | hide_urls | tr -d '"')
version=$(cast call --rpc-url "$ETH_RPC_URL" "$token" 'version()(string)' 2>&1 | hide_urls | tr -d '"')
separator=$(cast keccak "$(cast abi-encode 'f(bytes32,bytes32,bytes32,uint256,address)' \
    "$(cast keccak 'EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)')" \
    "$(cast keccak "$name")" "$(cast keccak "$version")" 11155111 "$token")")
check "DOMAIN_SEPARATOR()" "$(read_back "$token" 'DOMAIN_SEPARATOR()(bytes32)')" "$separator"

python3 - "$manifest" "$token" "$token_tx" "$token_block" "$token_gas" "$deposits" "$deposits_tx" \
    "$deposits_block" "$deposits_gas" "$deployer" "$railgun" "$zecswap" "$name" "$version" \
    "$(git rev-parse HEAD)" <<'EOF'
import json, sys, time
(path, token, token_tx, token_block, token_gas, deposits, deposits_tx, deposits_block,
 deposits_gas, deployer, railgun, zecswap, name, version, commit) = sys.argv[1:]
manifest = {
    "chainId": 11155111,
    "deposits": deposits,
    "token": token,
    "tokenName": name,
    "tokenVersion": version,
    "tokenDecimals": 6,
    "railgun": railgun,
    "sanctions": None,
    "contract": zecswap,
    "deployments": {
        "token": {"transaction": token_tx, "block": int(token_block), "gasUsed": int(token_gas)},
        "deposits": {
            "transaction": deposits_tx,
            "block": int(deposits_block),
            "gasUsed": int(deposits_gas),
        },
    },
    "deployer": deployer,
    "sourceCommit": commit,
    "deployedAt": time.strftime("%Y-%m-%d", time.gmtime()),
    "status": "staged",
}
with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
EOF
echo "Wrote $manifest. Next, on the droplet: switch-token.sh <release> $token $deposits"
