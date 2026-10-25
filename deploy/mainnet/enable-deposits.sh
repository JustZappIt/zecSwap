#!/usr/bin/env bash
# Turns mainnet deposits on (docs/deposits.md): the relayer gains `[deposits]`, sent through
# Flashbots Protect, and this release's binary, with the maker's and issuer's byte for byte; the
# mainnet gateway gains the deposits' allowance. On the droplet, as root, from a stage directory
# holding this script, relayer-deposits.toml, nginx-site.sed, deploy/nginx.conf and a Linux
# zecswap-relayer of this branch (scripts/build-server.sh), once deploy-deposits.sh has deployed
# the contract:
#   bash /root/stage-mainnet-deposits/enable-deposits.sh <release name> <Deposits contract>
# It waits until the mainnet maker's monitor shows no live swap, restarts the relayer alone and
# checks its terms. A failure puts everything back as it was and restarts the relayer again.
set -euo pipefail
umask 022

usage="usage: $0 <release name> <Deposits contract>"
release_name=${1:?$usage}
deposits=${2:?$usage}
stage=$(cd "$(dirname "$0")" && pwd)
usdc=0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48
railgun=0xfa7093cdd9ee6932b4eb2c9e1cde7ce00b1fa4b9
sanctions=0x40c57923924b5c5c5455c48d93317139addac8fb
release=/opt/zecswap-mainnet/releases/$release_name
backup=/var/backups/zecswap-mainnet/$release_name
relayer_config=/etc/zecswap-mainnet-relayer/config.toml
site=/etc/nginx/sites-available/zecswap-mainnet

die() {
    echo "$*" >&2
    exit 1
}
lower() { tr 'A-F' 'a-f'; }
[ "$(id -u)" = 0 ] || die "run as root"
[[ $deposits =~ ^0x[0-9a-fA-F]{40}$ ]] || die "$usage"
for file in zecswap-relayer relayer-deposits.toml nginx-site.sed nginx.conf; do
    [ -f "$stage/$file" ] || die "$stage/$file is missing"
done
[ ! -e "$release" ] || die "$release exists"
[ ! -e "$backup" ] || die "$backup exists"
grep -qx 'network = "mainnet"' "$relayer_config" || die "$relayer_config is not mainnet's"
grep -qix "token = \"$usdc\"" "$relayer_config" || die "the relayer's token is not USDC"
grep -qx '\[deposits\]' "$relayer_config" && die "the relayer takes deposits already"
systemctl is-active --quiet zecswap-mainnet-relayer || die "the mainnet relayer isn't running"
[ -L /etc/nginx/sites-enabled/zecswap-mainnet ] || die "no mainnet nginx site"
"$stage/zecswap-relayer" --help >/dev/null
previous=$(readlink -f /opt/zecswap-mainnet/current)

# The relayer's own RPC reads the contract before anything changes.
rpc=$(sed -n 's/^evm_rpc = "\(.*\)"$/\1/p' "$relayer_config")
call() {
    curl -fsS --max-time 30 -H 'Content-Type: application/json' "$rpc" \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$1\",\"data\":\"$2\"},\"latest\"]}" |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["result"])'
}
word() { printf '0x%064s' "${1#0x}" | tr ' ' 0 | lower; }
[ "$(call "$deposits" 0x82bfefc8)" = "$(word "$usdc")" ] || die "the Deposits contract takes another token"
[ "$(call "$deposits" 0x6baa4a21)" = "$(word "$railgun")" ] || die "the Deposits contract shields elsewhere"
[ "$(call "$deposits" 0x36574dad)" = "$(word "$sanctions")" ] ||
    die "the Deposits contract screens against another oracle"

# A restart never cuts a swap's relayer request off: wait for none live.
monitor_token=$(sed -n 's/^MAKER_MONITOR_TOKEN=//p' /etc/zecswap-mainnet-maker/secrets.env)
live() {
    curl -fsS --max-time 30 -H "Authorization: Bearer $monitor_token" \
        http://127.0.0.1:8797/v1/monitor | python3 -c 'import json, sys
counts = json.load(sys.stdin)["counts"]
print(counts["activeForward"] + counts["activeReverse"])'
}
until [ "$(live)" = 0 ]; do
    echo "$(date -u +%H:%M) the mainnet maker has live swaps; waiting (Ctrl-C changes nothing)"
    sleep 60
done

install -d -m 0700 "$backup"
cp -a "$relayer_config" "$backup/relayer-config.toml"
cp -a "$site" "$backup/nginx-site"
echo "$previous" >"$backup/previous-current"
restore() {
    trap - ERR
    echo "Failed: putting everything back and restarting the relayer." >&2
    cp -a "$backup/relayer-config.toml" "$relayer_config"
    cp -a "$backup/nginx-site" "$site"
    nginx -t && systemctl reload nginx
    ln -sfn "$previous" /opt/zecswap-mainnet/current
    systemctl restart zecswap-mainnet-relayer
}
trap restore ERR

{
    echo
    sed "s/<deposits>/$deposits/" "$stage/relayer-deposits.toml"
} >>"$relayer_config"
install -d -m 0755 "$release"
install -m 0755 "$stage/zecswap-relayer" "$release/"
cp -a "$previous/zecswap-maker" "$previous/zecswap-issuer" "$release/"
sed -f "$stage/nginx-site.sed" "$stage/nginx.conf" >"$site"
nginx -t
systemctl reload nginx
ln -sfn "$release" /opt/zecswap-mainnet/current
systemctl restart zecswap-mainnet-relayer

terms=
for _ in $(seq 60); do
    terms=$(curl -fsS --max-time 10 http://127.0.0.1:8798/v1/terms 2>/dev/null) && break
    sleep 2
done
python3 -c 'import json, sys
terms = json.loads(sys.argv[1])["deposits"]
assert terms["contract"].lower() == sys.argv[2].lower(), terms
assert terms["token"].lower() == sys.argv[3], terms
print("deposits go from", terms["relayer"], "for a fee of", terms["fee"], "and", terms["feeBps"],
      "bps, at least", terms["minValue"])' "$terms" "$deposits" "$usdc"
trap - ERR
echo "Deposits on: release $release. The relayer tops the deposits account above up from its own"
echo "account's ETH; record it in docs/local/deployment.md, and the account in the wallets' pins."
