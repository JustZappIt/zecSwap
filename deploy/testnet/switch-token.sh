#!/usr/bin/env bash
# Moves the hosted testnet onto the tUSD that deposits take, and turns deposits on
# (docs/deposits.md). The swap contract stays: it holds inventory per token, and each swap keeps
# its own token. On the droplet, as root, from a stage directory holding this script,
# relayer-deposits.toml, deploy/nginx.conf and a Linux zecswap-relayer of this branch
# (scripts/build-server.sh), once deploy-deposits.sh has deployed the token and the contract:
#   bash /root/stage-deposits/switch-token.sh <release name> <new token> <Deposits contract> [inventory]
# It waits until the maker's monitor shows no live swap, stops the maker and the relayer (the
# relayer serves one token, and would refuse swaps in the old one), moves the maker's inventory
# from the old tUSD to the new one, minting `inventory` base units of it if given (at least what
# it held of the old), switches both configs to the new token, gives the relayer
# `[deposits]`, this release's binary and the gateway's deposit allowance, starts both and checks
# them. Until the inventory moves, a failure puts everything back as it was and starts both again;
# after, it stops and says what is left to do.
set -euo pipefail
umask 022

usage="usage: $0 <release name> <new token> <Deposits contract> [inventory]"
release_name=${1:?$usage}
new_token=${2:?$usage}
deposits=${3:?$usage}
minted=${4:-}
stage=$(cd "$(dirname "$0")" && pwd)
old_token=0x5764D0044bef5AA839E0dDafE2073421101B9Ed8
zecswap=0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c
railgun=0xecfcf3b4ec647c4ca6d49108b311b7a7c9543fea
release=/opt/zecswap/releases/$release_name
backup=/var/backups/zecswap/$release_name
maker_config=/etc/zecswap-maker/config.toml
relayer_config=/etc/zecswap-relayer/config.toml

die() {
    echo "$*" >&2
    exit 1
}
lower() { tr 'A-F' 'a-f'; }
[ "$(id -u)" = 0 ] || die "run as root"
[[ $new_token =~ ^0x[0-9a-fA-F]{40}$ && $deposits =~ ^0x[0-9a-fA-F]{40}$ ]] || die "$usage"
for file in zecswap-relayer relayer-deposits.toml nginx.conf; do
    [ -f "$stage/$file" ] || die "$stage/$file is missing"
done
[ ! -e "$release" ] || die "$release exists"
[ ! -e "$backup" ] || die "$backup exists"
grep -qix "token = \"$old_token\"" "$maker_config" || die "the maker's token is not the old tUSD"
grep -qix "token = \"$old_token\"" "$relayer_config" || die "the relayer's token is not the old tUSD"
grep -qx '\[deposits\]' "$relayer_config" && die "the relayer takes deposits already"
grep -qix "contract = \"$zecswap\"" "$relayer_config" || die "the relayer serves another contract"
systemctl is-active --quiet zecswap-maker zecswap-relayer || die "the maker or relayer isn't running"
"$stage/zecswap-relayer" --help >/dev/null
previous=$(readlink -f /opt/zecswap/current)
site=$(grep -l 'listen 127.0.0.1:8080;' /etc/nginx/sites-available/* | head -n1)
[ -n "$site" ] && [ -L "/etc/nginx/sites-enabled/$(basename "$site")" ] || die "no testnet nginx site"

# The relayer's own RPC reads the new contracts before anything changes.
rpc=$(sed -n 's/^evm_rpc = "\(.*\)"$/\1/p' "$relayer_config")
call() {
    curl -fsS --max-time 30 -H 'Content-Type: application/json' "$rpc" \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$1\",\"data\":\"$2\"},\"latest\"]}" |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["result"])'
}
word() { printf '0x%064s' "${1#0x}" | tr ' ' 0 | lower; }
[ "$(call "$deposits" 0x82bfefc8)" = "$(word "$new_token")" ] || die "the Deposits contract takes another token"
[ "$(call "$deposits" 0x6baa4a21)" = "$(word "$railgun")" ] || die "the Deposits contract shields elsewhere"
[ "$(call "$deposits" 0x36574dad)" = "$(word 0)" ] || die "the Deposits contract screens depositors"

# Live swaps settle in the token they opened with: wait for none.
# shellcheck disable=SC1091
. /etc/zecswap-maker/monitor.env
monitor() {
    curl -fsS --max-time 30 -H "Authorization: Bearer $MAKER_MONITOR_TOKEN" \
        http://127.0.0.1:8787/v1/monitor
}
live() {
    monitor | python3 -c 'import json, sys
counts = json.load(sys.stdin)["counts"]
print(counts["activeForward"] + counts["activeReverse"])'
}
until [ "$(live)" = 0 ]; do
    echo "$(date -u +%H:%M) the maker has live swaps; waiting (Ctrl-C changes nothing)"
    sleep 60
done
maker=$(curl -fsS --max-time 30 http://127.0.0.1:8787/v1/info |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["maker"])')
inventory=$(python3 -c 'import sys; print(int(sys.argv[1], 16))' \
    "$(call "$zecswap" "0xf7888aec$(word "$maker" | cut -c3-)$(word "$old_token" | cut -c3-)")")
[ "$inventory" -gt 0 ] || die "the maker holds no old tUSD inventory to move"
minted=${minted:-$inventory}
[[ $minted =~ ^[0-9]+$ ]] && [ "$minted" -ge "$inventory" ] ||
    die "the new inventory is in base units, at least the old's $inventory"

install -d -m 0700 "$backup"
cp -a "$maker_config" "$backup/maker-config.toml"
cp -a "$relayer_config" "$backup/relayer-config.toml"
cp -a "$site" "$backup/nginx-site"
echo "$site" >"$backup/nginx-site-path"
echo "$previous" >"$backup/previous-current"
restore() {
    trap - ERR
    echo "Failed before the inventory moved: putting everything back and starting both." >&2
    cp -a "$backup/maker-config.toml" "$maker_config"
    cp -a "$backup/relayer-config.toml" "$relayer_config"
    cp -a "$backup/nginx-site" "$site"
    nginx -t && systemctl reload nginx
    ln -sfn "$previous" /opt/zecswap/current
    systemctl start zecswap-maker zecswap-relayer
}
trap restore ERR

systemctl stop zecswap-maker zecswap-relayer
store=/var/lib/zecswap-maker/maker.sqlite
started=$(sqlite3 -readonly "$store" "SELECT (SELECT count(*) FROM swaps WHERE settled = 0) +
    (SELECT count(*) FROM reverse_swaps WHERE settled = 0)")
[ "$started" = 0 ] || { echo "a swap started meanwhile" >&2; false; }
sqlite3 "$store" ".backup '$backup/maker.sqlite'"

# The maker's own command line, as its service runs: its user, folder and environment.
run_maker() {
    local properties=(-p WorkingDirectory=/var/lib/zecswap-maker)
    local file
    for file in $(systemctl show -p EnvironmentFiles --value zecswap-maker | awk '{ print $1 }'); do
        properties+=(-p "EnvironmentFile=$file")
    done
    systemd-run --wait --pipe --collect --quiet --uid=zecswap-maker --gid=zecswap-maker \
        "${properties[@]}" -E RUST_LOG=info,zcash_client_backend=warn -E RAYON_NUM_THREADS=1 \
        "$previous/zecswap-maker" --config "$maker_config" "$@" </dev/null
}

# The relayer: the new token, deposits, and this release's binary with the maker's and issuer's
# byte for byte.
sed -i "s/^token = \"$old_token\"$/token = \"$new_token\"/I" "$relayer_config"
{
    echo
    sed "s/<deposits>/$deposits/" "$stage/relayer-deposits.toml"
} >>"$relayer_config"
install -d -m 0755 "$release"
install -m 0755 "$stage/zecswap-relayer" "$release/"
cp -a "$previous/zecswap-maker" "$previous/zecswap-issuer" "$release/"
cp "$stage/nginx.conf" "$site"
nginx -t
systemctl reload nginx

# From here on the chain changes, which no restore undoes.
trap - ERR
moved() {
    echo "Failed after $1. The services are stopped; the backups are in $backup." >&2
    echo "Left to do: $2" >&2
    exit 1
}
run_maker withdraw-inventory all ||
    moved "nothing moved on chain" "run this script's steps from withdraw-inventory on"
sed -i "s/^token = \"$old_token\"$/token = \"$new_token\"/I" "$maker_config"
run_maker add-inventory "$minted" --mint ||
    moved "withdrawing $inventory old tUSD to the maker" \
        "add-inventory $minted --mint under the switched maker config, then start both"
ln -sfn "$release" /opt/zecswap/current
systemctl start zecswap-relayer zecswap-maker

for _ in $(seq 60); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8787/healthz)" = 204 ] && break
    sleep 2
done
[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8787/healthz)" = 204 ] ||
    moved "starting the maker" "read journalctl -u zecswap-maker"
curl -fsS http://127.0.0.1:8787/v1/info | python3 -c 'import json, sys
info = json.load(sys.stdin)
assert info["token"].lower() == sys.argv[1].lower(), info' "$new_token"
terms=$(curl -fsS http://127.0.0.1:8788/v1/terms)
python3 -c 'import json, sys
terms = json.loads(sys.argv[1])["deposits"]
assert terms["contract"].lower() == sys.argv[2].lower(), terms
assert terms["token"].lower() == sys.argv[3].lower(), terms
print("deposits go from", terms["relayer"], "for a fee of", terms["fee"], "and", terms["feeBps"], "bps")' \
    "$terms" "$deposits" "$new_token"
echo "Switched: $inventory base units of $old_token withdrawn, $minted of $new_token minted and deposited; release $release."
echo "Record it in docs/deployment.md, and point the dashboard's BRIDGE_TESTNET_TOKEN at $new_token."
