#!/usr/bin/env bash
# Moves the hosted testnet's maker and relayer from the lock contract onto the windows contract
# (docs/reverse-flow.md, Windows). On the droplet, as root, from a stage directory holding this
# script and Linux zecswap-maker and zecswap-relayer binaries of this branch
# (scripts/build-server.sh), once the windows contract is deployed:
#   bash /root/stage-windows/switch-windows.sh <release name> <windows contract> <min window> <late window>
# It checks the contract has those windows, pays into Railgun's proxy and has no lock, and that the
# maker's timing fits them; waits until the maker's monitor shows no live swap; stops the maker and
# the relayer (builds of this branch refuse the lock contract, and the lock contract's swaps follow
# its rules); moves the maker's inventory from the old contract to the new one; switches both
# configs to it; gives the maker a fresh store, as a new contract needs; installs this release's
# maker and relayer with the issuer byte for byte; starts both and checks them. Until the
# inventory moves, a failure puts everything back as it was and starts both again; after, it stops
# and says what is left to do.
set -euo pipefail
umask 022

usage="usage: $0 <release name> <windows contract> <min window seconds> <late window seconds>"
release_name=${1:?$usage}
contract=${2:?$usage}
min_window=${3:?$usage}
late_window=${4:?$usage}
stage=$(cd "$(dirname "$0")" && pwd)
old_contract=0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c
token=0x64c4a5cb09fe0b706d4cf24dc0a45fff275d727c
railgun=0xecfcf3b4ec647c4ca6d49108b311b7a7c9543fea
release=/opt/zecswap/releases/$release_name
backup=/var/backups/zecswap/$release_name
maker_config=/etc/zecswap-maker/config.toml
relayer_config=/etc/zecswap-relayer/config.toml
store=/var/lib/zecswap-maker/maker.sqlite

die() {
    echo "$*" >&2
    exit 1
}
lower() { tr 'A-F' 'a-f'; }
[ "$(id -u)" = 0 ] || die "run as root"
[[ $contract =~ ^0x[0-9a-fA-F]{40}$ && $min_window =~ ^[0-9]+$ && $late_window =~ ^[0-9]+$ ]] || die "$usage"
for file in zecswap-maker zecswap-relayer; do
    [ -f "$stage/$file" ] || die "$stage/$file is missing"
    "$stage/$file" --help >/dev/null
done
[ ! -e "$release" ] || die "$release exists"
[ ! -e "$backup" ] || die "$backup exists"
grep -qix "contract = \"$old_contract\"" "$maker_config" || die "the maker serves another contract"
grep -qix "contract = \"$old_contract\"" "$relayer_config" || die "the relayer serves another contract"
grep -qix "token = \"$token\"" "$maker_config" || die "the maker's token is not tUSD"
systemctl is-active --quiet zecswap-maker zecswap-relayer || die "the maker or relayer isn't running"
previous=$(readlink -f /opt/zecswap/current)

# The relayer's own RPC reads the new contract before anything changes.
rpc=$(sed -n 's/^evm_rpc = "\(.*\)"$/\1/p' "$relayer_config")
call() {
    curl -fsS --max-time 30 -H 'Content-Type: application/json' "$rpc" \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$1\",\"data\":\"$2\"},\"latest\"]}" |
        python3 -c 'import json, sys
answer = json.load(sys.stdin)
print(answer.get("result") or "none")'
}
word() { printf '0x%064s' "${1#0x}" | tr ' ' 0 | lower; }
number() { python3 -c 'import sys; print(int(sys.argv[1], 16))' "$1"; }
[ "$(number "$(call "$contract" 0x52b65c4b)")" = "$min_window" ] || die "the contract's MIN_WINDOW isn't $min_window"
[ "$(number "$(call "$contract" 0x4d9fef3e)")" = "$late_window" ] || die "the contract's LATE_WINDOW isn't $late_window"
[ "$(call "$contract" 0x6baa4a21)" = "$(word "$railgun")" ] || die "the contract pays into another Railgun"
[ "$(call "$contract" 0x485d3834)" = none ] || die "the contract has a LOCK_DURATION: it is the lock contract"
[ "$(call "$old_contract" 0x485d3834)" != none ] || die "the old contract has no LOCK_DURATION"

# What the maker checks at start against the windows, checked before anything stops.
python3 - "$maker_config" "$min_window" "$late_window" <<'EOF'
import sys, tomllib
config, least, late = tomllib.load(open(sys.argv[1], "rb")), int(sys.argv[2]), int(sys.argv[3])
timing, reverse = config["timing"], config.get("reverse")
assert timing["t0_after"] >= least + 600, "t0_after leaves no minimum window and open inclusion"
assert timing["t1_after"] - timing["t0_after"] >= least, "t1_after follows t0 by less than the minimum window"
assert timing["reveal_margin"] + timing["tick"] < timing["t0_margin"], "t0_margin leaves no cancel"
assert timing["reveal_margin"] + timing["tick"] < late, "the late window leaves no refund"
assert not reverse or (reverse["ready_after"] - reverse["funding_window"] >= least
    and reverse["refund_after"] - reverse["ready_after"] >= least), "reverse deadlines under the minimum window"
EOF

# Live swaps settle on the contract they opened on: wait for none.
# shellcheck disable=SC1091
. /etc/zecswap-maker/monitor.env
live() {
    curl -fsS --max-time 30 -H "Authorization: Bearer $MAKER_MONITOR_TOKEN" http://127.0.0.1:8787/v1/monitor |
        python3 -c 'import json, sys
counts = json.load(sys.stdin)["counts"]
print(counts["activeForward"] + counts["activeReverse"])'
}
until [ "$(live)" = 0 ]; do
    echo "$(date -u +%H:%M) the maker has live swaps; waiting (Ctrl-C changes nothing)"
    sleep 60
done
maker=$(curl -fsS --max-time 30 http://127.0.0.1:8787/v1/info |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["maker"])')
inventory=$(number "$(call "$old_contract" "0xf7888aec$(word "$maker" | cut -c3-)$(word "$token" | cut -c3-)")")
[ "$inventory" -gt 0 ] || die "the maker holds no inventory on the old contract to move"

install -d -m 0700 "$backup"
cp -a "$maker_config" "$backup/maker-config.toml"
cp -a "$relayer_config" "$backup/relayer-config.toml"
echo "$previous" >"$backup/previous-current"
restore() {
    trap - ERR
    echo "Failed before the inventory moved: putting everything back and starting both." >&2
    cp -a "$backup/maker-config.toml" "$maker_config"
    cp -a "$backup/relayer-config.toml" "$relayer_config"
    ln -sfn "$previous" /opt/zecswap/current
    systemctl start zecswap-maker zecswap-relayer
}
trap restore ERR

systemctl stop zecswap-maker zecswap-relayer
started=$(sqlite3 -readonly "$store" "SELECT (SELECT count(*) FROM swaps WHERE settled = 0) +
    (SELECT count(*) FROM reverse_swaps WHERE settled = 0)")
[ "$started" = 0 ] || { echo "a swap started meanwhile" >&2; false; }
sqlite3 "$store" ".backup '$backup/maker.sqlite'"

# The maker's own command line, as its service runs: its user, folder and environment.
run_maker() {
    local binary=$1
    shift
    local properties=(-p WorkingDirectory=/var/lib/zecswap-maker)
    local file
    for file in $(systemctl show -p EnvironmentFiles --value zecswap-maker | awk '{ print $1 }'); do
        properties+=(-p "EnvironmentFile=$file")
    done
    systemd-run --wait --pipe --collect --quiet --uid=zecswap-maker --gid=zecswap-maker \
        "${properties[@]}" -E RUST_LOG=info,zcash_client_backend=warn -E RAYON_NUM_THREADS=1 \
        "$binary" --config "$maker_config" "$@" </dev/null
}

install -d -m 0755 "$release"
install -m 0755 "$stage/zecswap-maker" "$stage/zecswap-relayer" "$release/"
cp -a "$previous/zecswap-issuer" "$release/"

# From here on the chain changes, which no restore undoes.
trap - ERR
moved() {
    echo "Failed after $1. The services are stopped; the backups are in $backup." >&2
    echo "Left to do: $2" >&2
    exit 1
}
run_maker "$previous/zecswap-maker" withdraw-inventory all ||
    moved "nothing moved on chain" "run this script's steps from withdraw-inventory on"
sed -i "s/^contract = \"$old_contract\"$/contract = \"$contract\"/I" "$maker_config" "$relayer_config"
# A new contract's maker starts on a store of its own; the wallets and spent tokens stay.
mv "$store" "$backup/retired-maker.sqlite"
run_maker "$release/zecswap-maker" add-inventory "$inventory" ||
    moved "withdrawing $inventory tUSD to the maker" \
        "add-inventory $inventory under the switched maker config with $release/zecswap-maker, then start both"
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
assert info["contract"].lower() == sys.argv[1].lower(), info' "$contract"
curl -fsS http://127.0.0.1:8788/v1/terms | python3 -c 'import json, sys
terms = json.load(sys.stdin)
assert terms["contract"].lower() == sys.argv[1].lower(), terms' "$contract"
echo "Switched: $inventory base units of tUSD moved from $old_contract to $contract; release $release."
echo "Record it in docs/deployment.md, and point the dashboard's BRIDGE_TESTNET_CONTRACT at $contract."
