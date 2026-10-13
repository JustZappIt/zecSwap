#!/usr/bin/env bash
# Generates every secret the mainnet maker and relayer need, on this machine, and prints only
# their public addresses. Writes, each 0600, into <out-dir>:
#   maker.env, relayer.env  the hosts' /etc/zecswap-maker/secrets.env and
#                           /etc/zecswap-relayer/secrets.env, once you fill in the API keys
#   recovery.txt            what to keep offline: the maker's Zcash phrase (restores its ZEC
#                           inventory in any Zcash wallet), the relayer's Railgun phrase (its fees,
#                           in any Railgun wallet) and both Ethereum keys
# Keep <out-dir> on an encrypted disk, copy recovery.txt somewhere offline, and delete the env
# files once they are on the host.
#   scripts/mainnet-secrets.sh ~/zecswap-mainnet-secrets
set -euo pipefail
umask 077

out=${1:?usage: $0 <out-dir>}
for file in maker.env relayer.env recovery.txt; do
    if [ -e "$out/$file" ]; then
        echo "$out/$file already exists: nothing was written" >&2
        exit 1
    fi
done
mkdir -p "$out"
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

# Railgun's and Zcash's wallets take BIP-39 seeds: a phrase's seed opens the same keys there.
phrase() {
    cast wallet new-mnemonic --words 24 | awk '/^Phrase:/ { getline; print; exit }'
}
seed() {
    python3 -c 'import hashlib, sys, unicodedata
phrase = unicodedata.normalize("NFKD", sys.argv[1])
print(hashlib.pbkdf2_hmac("sha512", phrase.encode(), b"mnemonic", 2048).hex())' "$1"
}
evm_key() {
    cast wallet new --json | python3 -c 'import json, sys
key = json.load(sys.stdin)[0]
print(key["address"], key["private_key"])'
}

read -r maker_address maker_key <<<"$(evm_key)"
read -r relayer_address relayer_key <<<"$(evm_key)"
maker_key=${maker_key#0x}
relayer_key=${relayer_key#0x}
zcash_phrase=$(phrase)
railgun_phrase=$(phrase)
[ "$(wc -w <<<"$zcash_phrase")" -eq 24 ] && [ "$(wc -w <<<"$railgun_phrase")" -eq 24 ] || {
    echo "cast printed no phrase: nothing was written" >&2
    exit 1
}
root_secret=$(openssl rand -hex 32)

cat >"$out/maker.env" <<EOF
MAKER_PRIVATE_KEY=$maker_key
MAKER_ROOT_SECRET=$root_secret
MAKER_TICKET_SECRET=$(openssl rand -hex 32)
MAKER_ZCASH_SEED=$(seed "$zcash_phrase")
MAKER_MONITOR_TOKEN=$(openssl rand -hex 32)
ZCASH_CMC_KEY=<CoinMarketCap key>
ALCHEMY_API_KEY=<the mainnet Alchemy app's key>
EOF
cat >"$out/relayer.env" <<EOF
RELAYER_PRIVATE_KEY=$relayer_key
RELAYER_RAILGUN_SEED=$(seed "$railgun_phrase")
RELAYER_MONITOR_TOKEN=$(openssl rand -hex 32)
ZCASH_CMC_KEY=<CoinMarketCap key>
ALCHEMY_API_KEY=<the mainnet Alchemy app's key>
EOF
cat >"$out/recovery.txt" <<EOF
ZecSwap mainnet, generated $(date -u +%Y-%m-%dT%H:%M:%SZ)

Maker Ethereum account $maker_address
  private key: $maker_key
  Holds the maker's ETH; its USDC inventory sits in the swap contract under this account.

Maker root secret: $root_secret
  Every maker share derives from it: with the maker's store it settles any swap after a loss.

Maker ZEC inventory (Zcash, ZIP 32 account 0 of this phrase):
  $zcash_phrase

Relayer Ethereum account $relayer_address
  private key: $relayer_key

Relayer Railgun wallet (its fees):
  $railgun_phrase
EOF

echo "Wrote $out/maker.env, $out/relayer.env and $out/recovery.txt."
echo "Maker account:   $maker_address"
echo "Relayer account: $relayer_address"
echo "Fill in ZCASH_CMC_KEY and ALCHEMY_API_KEY in both env files."
