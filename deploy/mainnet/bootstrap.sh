#!/usr/bin/env bash
# Sets up a fresh Ubuntu 24.04 host for the mainnet maker, relayer and token issuer, from a stage
# directory that holds this script, the three Linux binaries (scripts/build-server.sh),
# deploy/systemd/ and deploy/nginx.conf. Run as root:
#   bash /root/stage/bootstrap.sh <release name>
# It installs the release and what surrounds it and starts nothing: it ends by listing the configs
# and secrets to put in place before starting the services.
set -euo pipefail
umask 022

stage=$(cd "$(dirname "$0")" && pwd)
release=/opt/zecswap/releases/${1:?usage: $0 <release name>}

if [ "$(id -u)" != 0 ]; then
    echo "run as root" >&2
    exit 1
fi
# Safe to run again after a failure, never under running services.
if systemctl is-active --quiet zecswap-maker zecswap-relayer zecswap-issuer; then
    echo "a ZecSwap service is running: this script sets up a fresh host" >&2
    exit 1
fi
for file in zecswap-maker zecswap-relayer zecswap-issuer nginx.conf systemd/zecswap-maker.service; do
    if [ ! -f "$stage/$file" ]; then
        echo "$stage/$file is missing" >&2
        exit 1
    fi
done

export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -yq nginx sqlite3 curl ufw unattended-upgrades
if ! command -v cloudflared >/dev/null; then
    curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg -o /usr/share/keyrings/cloudflare-main.gpg
    echo 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main' \
        >/etc/apt/sources.list.d/cloudflared.list
    apt-get update -q
    apt-get install -yq cloudflared
fi

# Only SSH comes in: the services listen on loopback, and requests arrive through the tunnel.
ufw default deny incoming
ufw default allow outgoing
ufw allow OpenSSH
ufw --force enable

# Halo 2 proving peaks above a small droplet's memory.
if [ -z "$(swapon --show)" ]; then
    fallocate -l 2G /swapfile
    chmod 600 /swapfile
    mkswap /swapfile
    swapon /swapfile
    echo '/swapfile none swap sw 0 0' >>/etc/fstab
fi

for service in maker relayer issuer; do
    id "zecswap-$service" >/dev/null 2>&1 ||
        useradd --system --no-create-home --shell /usr/sbin/nologin "zecswap-$service"
    install -d -m 0750 -o root -g "zecswap-$service" "/etc/zecswap-$service"
done
install -d -m 0700 /etc/zecswap-tunnel /var/backups/zecswap
install -d -m 0755 /opt/zecswap /opt/zecswap/releases "$release"
install -m 0755 "$stage"/zecswap-maker "$stage"/zecswap-relayer "$stage"/zecswap-issuer "$release/"
ln -sfn "$release" /opt/zecswap/current

# The token issuer: Google's attestation roots, its key and the maker's key for handing tokens
# back. Keys are made once and never overwritten; their public halves go into the maker's
# [tokens] and the app.
install -d -m 0750 -o root -g zecswap-issuer /etc/zecswap-issuer/roots
curl -fsS --max-time 60 https://android.googleapis.com/attestation/root | python3 -c '
import json, sys
for n, pem in enumerate(json.load(sys.stdin), 1):
    with open(f"/etc/zecswap-issuer/roots/google-{n}.pem", "w") as f:
        f.write(pem.strip() + "\n")'
chgrp zecswap-issuer /etc/zecswap-issuer/roots/*.pem
chmod 0640 /etc/zecswap-issuer/roots/*.pem
keygen() {
    local path=$1 group=$2
    if [ ! -e "$path" ]; then
        echo "$path's public half:"
        "$release/zecswap-issuer" keygen "$path"
        chown "root:$group" "$path"
        chmod 0640 "$path"
    fi
}
keygen /etc/zecswap-issuer/key.pem zecswap-issuer
keygen /etc/zecswap-maker/return-key.pem zecswap-maker

install -m 0644 "$stage"/systemd/*.service "$stage"/systemd/*.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable zecswap-maker zecswap-relayer zecswap-issuer zecswap-tunnel
systemctl enable --now zecswap-issuer-status.timer
systemctl start zecswap-issuer-status.service ||
    echo "Google's status list didn't download; the timer retries, and the issuer waits for it" >&2

install -m 0644 "$stage/nginx.conf" /etc/nginx/sites-available/zecswap
ln -sfn /etc/nginx/sites-available/zecswap /etc/nginx/sites-enabled/zecswap
rm -f /etc/nginx/sites-enabled/default
nginx -t
systemctl reload nginx

cat <<EOF
Installed $release. Put these in place before starting anything:
  /etc/zecswap-maker/secrets.env    root:root 0600              scripts/mainnet-secrets.sh's maker.env
  /etc/zecswap-maker/config.toml    root:zecswap-maker 0640     deploy/mainnet/maker.toml, filled in,
                                                                [tokens] with the issuer key's half
  /etc/zecswap-relayer/secrets.env  root:root 0600              its relayer.env
  /etc/zecswap-relayer/config.toml  root:zecswap-relayer 0640   deploy/mainnet/relayer.toml, filled in
  /etc/zecswap-issuer/config.toml   root:zecswap-issuer 0640    deploy/mainnet/issuer.toml, with the
                                                                app build's signing digest
  /etc/zecswap-tunnel/token         root:root 0600              the mainnet tunnel's token
Then: systemctl start zecswap-tunnel zecswap-issuer zecswap-relayer zecswap-maker
EOF
