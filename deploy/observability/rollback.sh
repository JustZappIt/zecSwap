#!/bin/sh
set -eu
# Only collector components: never restart maker, relayer, tunnel, or databases.
systemctl disable --now zecswap-alloy-guard.timer alloy.service
rm -f /etc/alloy/ENABLED
python3 /opt/zecswap-observability/admin_access.py --remove
echo 'Alloy stopped; its RAM-only buffer is gone. Bridge services were untouched.'
