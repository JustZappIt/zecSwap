#!/usr/bin/env python3
"""Run as root on the VPS. Never prints credential values or starts services."""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import stat
import tomllib
from urllib.parse import urlsplit


def protected_write(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".new")
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as out:
        out.write(value)
    temporary.chmod(0o600)
    temporary.replace(path)


def known_secrets():
    secrets = set()
    for directory in ("/etc/zecswap-maker", "/etc/zecswap-relayer"):
        for path in Path(directory).glob("*.env"):
            for line in path.read_text().splitlines():
                if "=" not in line or line.lstrip().startswith("#"):
                    continue
                key, value = line.split("=", 1)
                if not re.search(r"KEY|TOKEN|SECRET|SEED|PASSWORD|CHAT_ID", key):
                    continue
                words = shlex.split(value, comments=True)
                if words and len(words[0]) >= 8:
                    secrets.add(words[0])
                    if re.fullmatch(r"0x[0-9a-fA-F]{64}", words[0]):
                        secrets.add(words[0][2:])
        for path in Path(directory).glob("*.toml"):
            def walk(value):
                if isinstance(value, dict):
                    for child in value.values():
                        walk(child)
                elif isinstance(value, str) and value.startswith(("https://", "http://")):
                    u = urlsplit(value)
                    if "alchemy" in (u.hostname or "") or u.password:
                        secrets.add(value)
                        if "/v2/" in u.path:
                            secrets.add(u.path.split("/v2/", 1)[1])
                        if u.password:
                            secrets.add(u.password)
            walk(tomllib.loads(path.read_text()))
    token = Path("/etc/alloy/ingestion.token")
    if token.exists():
        secrets.add(token.read_text().strip())
    assert secrets, "No bridge secrets found; refusing an unprotected pipeline"
    # One unnamed capture; no secret extraction into metadata.
    return "(" + "|".join(re.escape(s) for s in sorted(secrets, key=len, reverse=True)) + ")"


def cloud_config(source):
    path = Path(source)
    st = path.stat()
    assert st.st_uid == 0 and stat.S_IMODE(st.st_mode) == 0o600, "Use root ownership and mode 0600"
    data = json.loads(path.read_text())
    assert data["plan"] == "free", "This deployment only authorizes the Free plan"
    for key in ("stack_url", "logs_url"):
        u = urlsplit(data[key])
        assert u.scheme == "https" and u.hostname and u.hostname.endswith(".grafana.net"), "Expected Grafana Cloud HTTPS URL"
        assert not u.username and not u.password and not u.query and not u.fragment
    assert data["logs_url"].endswith("/loki/api/v1/push")
    for key in ("logs_username",):
        assert str(data[key]).isdigit(), "Expected numeric Cloud tenant ID"
    assert len(data["ingestion_token"]) > 20 and not re.search(r"\s", data["ingestion_token"])
    protected_write("/etc/alloy/ingestion.token", data["ingestion_token"])
    env = {"GRAFANA_LOGS_URL": data["logs_url"], "GRAFANA_LOGS_USERNAME": str(data["logs_username"])}
    protected_write("/etc/alloy/cloud.env", "".join(f"{k}={v}\n" for k, v in env.items()))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--cloud-file")
    args = parser.parse_args()
    assert os.geteuid() == 0, "Run on the VPS as root"
    Path("/etc/alloy").mkdir(mode=0o755, exist_ok=True)
    if args.cloud_file:
        cloud_config(args.cloud_file)
    protected_write("/etc/alloy/redact.regex", known_secrets())
    print("Protected credential/redaction files prepared. Service state unchanged.")
