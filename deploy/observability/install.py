#!/usr/bin/env python3
"""Install the prepared configuration after installing the pinned Alloy binary.

Does not enable/start Alloy, modify bridge units, or install any other software.
"""
import datetime
import os
from pathlib import Path
import pwd
import shutil
import subprocess

assert os.geteuid() == 0
base = Path(__file__).resolve().parent
assert base == Path("/opt/zecswap-observability"), "Copy this directory to /opt/zecswap-observability first"
version = subprocess.check_output(["/usr/local/bin/alloy", "--version"], text=True)
assert "version v1.20.1 " in version, "Validate a version upgrade separately"
user = pwd.getpwnam("alloy")
unit = (base / "alloy.service").read_text().replace("@ALLOY_UID@", str(user.pw_uid)).replace("@ALLOY_GID@", str(user.pw_gid))
files = {
    Path("/etc/alloy/config.alloy"): (base / "config.alloy").read_text(),
    Path("/etc/systemd/system/alloy.service"): unit,
    **{Path("/etc/systemd/system") / name: (base / name).read_text() for name in ["zecswap-alloy-guard.service", "zecswap-alloy-guard.timer"]},
}
stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
backup = Path("/root") / ("alloy-config-backup-" + stamp)
for path, content in files.items():
    if path.exists() and path.read_text() != content:
        backup.mkdir(mode=0o700, exist_ok=True)
        shutil.copy2(path, backup / path.name)
    path.parent.mkdir(exist_ok=True)
    path.write_text(content)
    path.chmod(0o644)
subprocess.run(["systemctl", "daemon-reload"], check=True)
print("Alloy configuration and watchdog installed; services were not started.")
