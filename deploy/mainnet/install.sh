#!/usr/bin/env bash
# Installs the mainnet maker, relayer and token issuer beside the testnet services on the droplet,
# under names of their own: units zecswap-mainnet-*, users and /etc, /var/lib folders of the same
# names, releases in /opt/zecswap-mainnet, the APIs on 127.0.0.1:8797-8799 behind nginx on
# 127.0.0.1:8090, which the droplet's tunnel reaches through a Workers VPC service of its own.
# Nothing of testnet's is touched. From a stage directory holding this script, nginx-site.sed, the
# three Linux binaries (scripts/build-server.sh), deploy/systemd/ and deploy/nginx.conf, as root:
#   bash /root/stage-mainnet/install.sh <release name>
# It starts no service: it ends by listing what to put in place before starting them.
set -euo pipefail
umask 022

stage=$(cd "$(dirname "$0")" && pwd)
release=/opt/zecswap-mainnet/releases/${1:?usage: $0 <release name>}

if [ "$(id -u)" != 0 ]; then
    echo "run as root" >&2
    exit 1
fi
for file in zecswap-maker zecswap-relayer zecswap-issuer nginx.conf nginx-site.sed \
    systemd/zecswap-maker.service systemd/zecswap-relayer.service systemd/zecswap-issuer.service \
    systemd/zecswap-issuer-status.service systemd/zecswap-issuer-status.timer; do
    if [ ! -f "$stage/$file" ]; then
        echo "$stage/$file is missing" >&2
        exit 1
    fi
done
# Safe to run again after a failure, never under running services.
if systemctl is-active --quiet zecswap-mainnet-maker zecswap-mainnet-relayer zecswap-mainnet-issuer; then
    echo "mainnet services are running: install a release with its own script instead" >&2
    exit 1
fi

# Two makers proving at once outgrow a 1 GB droplet: keep 3 GB of swap.
if [ "$(free -m | awk '/^Swap:/ { print $2 }')" -lt 3000 ] && [ ! -e /swapfile-mainnet ]; then
    fallocate -l 2G /swapfile-mainnet
    chmod 600 /swapfile-mainnet
    mkswap /swapfile-mainnet
    swapon /swapfile-mainnet
    echo '/swapfile-mainnet none swap sw 0 0' >>/etc/fstab
fi

for service in maker relayer issuer; do
    user=zecswap-mainnet-$service
    id "$user" >/dev/null 2>&1 ||
        useradd --system --no-create-home --shell /usr/sbin/nologin "$user"
    install -d -m 0750 -o root -g "$user" "/etc/$user"
done
install -d -m 0755 /opt/zecswap-mainnet /opt/zecswap-mainnet/releases "$release"
install -m 0755 "$stage"/zecswap-maker "$stage"/zecswap-relayer "$stage"/zecswap-issuer "$release/"
ln -sfn "$release" /opt/zecswap-mainnet/current

# Google's attestation roots, the issuer's key and the maker's key for handing tokens back. Keys
# are made once and never overwritten; their public halves go into the maker's [tokens] and the app.
install -d -m 0750 -o root -g zecswap-mainnet-issuer /etc/zecswap-mainnet-issuer/roots
curl -fsS --max-time 60 https://android.googleapis.com/attestation/root | python3 -c '
import json, sys
for n, pem in enumerate(json.load(sys.stdin), 1):
    with open(f"/etc/zecswap-mainnet-issuer/roots/google-{n}.pem", "w") as f:
        f.write(pem.strip() + "\n")'
chgrp zecswap-mainnet-issuer /etc/zecswap-mainnet-issuer/roots/*.pem
chmod 0640 /etc/zecswap-mainnet-issuer/roots/*.pem
keygen() {
    local path=$1 group=$2
    if [ ! -e "$path" ]; then
        echo "$path's public half:"
        "$release/zecswap-issuer" keygen "$path"
        chown "root:$group" "$path"
        chmod 0640 "$path"
    fi
}
keygen /etc/zecswap-mainnet-issuer/key.pem zecswap-mainnet-issuer
keygen /etc/zecswap-mainnet-maker/return-key.pem zecswap-mainnet-maker

# Testnet's units, with mainnet's names, users and folders.
mainnet_unit() {
    sed -e 's#^Description=ZecSwap #Description=ZecSwap mainnet #' \
        -e 's#^\(User\|Group\|StateDirectory\)=zecswap-#\1=zecswap-mainnet-#' \
        -e 's#/var/lib/zecswap-#/var/lib/zecswap-mainnet-#g' \
        -e 's#/etc/zecswap-#/etc/zecswap-mainnet-#g' \
        -e 's#/opt/zecswap/current/#/opt/zecswap-mainnet/current/#g' "$1"
}
for unit in maker.service relayer.service issuer.service issuer-status.service issuer-status.timer; do
    mainnet_unit "$stage/systemd/zecswap-$unit" >"/etc/systemd/system/zecswap-mainnet-$unit"
done
# Telegram messages go to the testnet's chat: each names its network.
if [ -f /etc/zecswap-maker/telegram.env ]; then
    for service in maker relayer; do
        install -d -m 0755 "/etc/systemd/system/zecswap-mainnet-$service.service.d"
        printf '[Service]\nEnvironmentFile=/etc/zecswap-maker/telegram.env\n' \
            >"/etc/systemd/system/zecswap-mainnet-$service.service.d/telegram.conf"
    done
fi
systemctl daemon-reload
systemctl enable zecswap-mainnet-maker zecswap-mainnet-relayer zecswap-mainnet-issuer
systemctl enable --now zecswap-mainnet-issuer-status.timer
systemctl start zecswap-mainnet-issuer-status.service ||
    echo "Google's status list didn't download; the timer retries, and the issuer waits for it" >&2

# Testnet's gateway, with zones and variables of its own, on its own port.
site=/etc/nginx/sites-available/zecswap-mainnet
sed -f "$stage/nginx-site.sed" "$stage/nginx.conf" >"$site"
ln -sfn "$site" /etc/nginx/sites-enabled/zecswap-mainnet
if ! nginx -t; then
    rm -f /etc/nginx/sites-enabled/zecswap-mainnet
    echo "nginx refused the mainnet site; it is unlinked and testnet's gateway is as it was" >&2
    exit 1
fi
systemctl reload nginx

cat <<EOF
Installed $release. Put these in place, then start the services:
  /etc/zecswap-mainnet-maker/secrets.env    root:root 0600                    scripts/mainnet-secrets.sh's maker.env
  /etc/zecswap-mainnet-maker/config.toml    root:zecswap-mainnet-maker 0640   deploy/mainnet/maker.toml, filled in
  /etc/zecswap-mainnet-relayer/secrets.env  root:root 0600                    its relayer.env
  /etc/zecswap-mainnet-relayer/config.toml  root:zecswap-mainnet-relayer 0640 deploy/mainnet/relayer.toml, filled in
  /etc/zecswap-mainnet-issuer/config.toml   root:zecswap-mainnet-issuer 0640  deploy/mainnet/issuer.toml, filled in
  systemctl start zecswap-mainnet-issuer zecswap-mainnet-relayer zecswap-mainnet-maker
EOF
