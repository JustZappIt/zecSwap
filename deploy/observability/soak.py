#!/usr/bin/env python3
"""Bounded on-VPS test: real journal logs go only to a loopback sink."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pwd
import re
import sqlite3
import subprocess
import threading
import time

from guard import sample
from wire import log_entries, snappy


def height(path):
    try:
        with sqlite3.connect("file:" + path + "?mode=ro", uri=True, timeout=0.1) as conn:
            return conn.execute("SELECT max(height) FROM blocks").fetchone()[0]
    except sqlite3.Error:
        return None


def run(seconds):
    base = Path("/opt/zecswap-observability")
    work = Path("/run/zecswap-alloy-soak")
    work.mkdir(mode=0o700, exist_ok=True)
    state = {"log_batches": 0, "log_entries": 0, "services": set(), "errors": [], "samples": []}
    secret_pattern = re.compile(Path("/etc/alloy/redact.regex").read_text())

    class Sink(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            if length > 4 * 1024 * 1024:
                self.send_error(413)
                return
            try:
                data = snappy(self.rfile.read(length))
                if self.path.endswith("/loki/api/v1/push"):
                    for labels, line in log_entries(data):
                        assert not secret_pattern.search(line), "known-secret-leak"
                        assert "swap_id=" not in labels and "transaction_hash=" not in labels, "indexed-id"
                        assert 'environment="testnet"' in labels, "missing-environment"
                        service = re.search(r'service="([^"]+)"', labels)
                        if service:
                            state["services"].add(service[1])
                        state["log_entries"] += 1
                    state["log_batches"] += 1
                self.send_response(204)
                self.end_headers()
            except Exception as exc:
                # No raw payloads, keys, log bodies, or Authorization headers are retained.
                state["errors"].append(type(exc).__name__)
                self.send_error(400)

    server = ThreadingHTTPServer(("127.0.0.1", 19090), Sink)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    config = Path("/etc/alloy/config.alloy").read_text()
    config = config.replace('sys.env("GRAFANA_LOGS_URL")', '"http://127.0.0.1:19090/loki/api/v1/push"')
    (base / "config.soak.alloy").write_text(config)
    (work / "token").write_text("SYNTHETIC_LOCAL_ONLY_INGESTION_TOKEN")
    (work / "cloud.env").write_text("GRAFANA_LOGS_USERNAME=1\n")
    user = pwd.getpwnam("alloy")
    unit = (base / "alloy.service").read_text()
    unit = re.sub(r"^ConditionPathExists=.*\n", "", unit, flags=re.M)
    unit = unit.replace("/etc/alloy/cloud.env", str(work / "cloud.env"))
    unit = unit.replace("/etc/alloy/ingestion.token", str(work / "token"))
    unit = unit.replace("/etc/alloy/config.alloy", str(base / "config.soak.alloy"))
    unit = unit.replace("@ALLOY_UID@", str(user.pw_uid)).replace("@ALLOY_GID@", str(user.pw_gid))
    Path("/run/systemd/system/alloy-canary.service").write_text(unit)
    subprocess.run(["systemctl", "daemon-reload"], check=True)
    initial = {p: height("/var/lib/zecswap-maker/" + p) for p in ["wallet.sqlite", "flow-wallet.sqlite"]}
    started = time.monotonic()
    try:
        subprocess.run(["systemctl", "start", "alloy-canary"], check=True)
        while time.monotonic() - started < seconds:
            row = sample("alloy-canary.service")
            pid = subprocess.check_output(["systemctl", "show", "alloy-canary", "-p", "MainPID", "--value"], text=True).strip()
            if pid == "0":
                state["errors"].append("collector-stopped")
                break
            st = os.statvfs(f"/proc/{pid}/root/run/alloy")
            row["buffer_mib"] = (st.f_blocks - st.f_bfree) * st.f_frsize / 1048576
            state["samples"].append(row)
            if row["available_mib"] < 192 or row["collector_mib"] > 120 or not row["maker_ok"] or not row["relayer_ok"] or row["buffer_mib"] > 6:
                state["errors"].append("availability-threshold")
                break
            time.sleep(15)
    finally:
        subprocess.run(["systemctl", "stop", "alloy-canary"], check=False)
        server.shutdown()
        state["elapsed_seconds"] = round(time.monotonic() - started, 1)
        state["initial_scan_height"] = initial
        state["final_scan_height"] = {p: height("/var/lib/zecswap-maker/" + p) for p in initial}
        state["services"] = sorted(state["services"])
        out = Path("/root/zecswap-alloy-soak-report.json")
        out.touch(mode=0o600)
        out.write_text(json.dumps(state, indent=2) + "\n")
        Path("/run/systemd/system/alloy-canary.service").unlink(missing_ok=True)
        subprocess.run(["systemctl", "daemon-reload"], check=True)
        (base / "config.soak.alloy").unlink(missing_ok=True)
        for p in work.iterdir():
            p.unlink()
        work.rmdir()
        print(json.dumps({k: v for k, v in state.items() if k != "samples"}))
        if state["samples"]:
            print(json.dumps({"peak_collector_mib": max(s["collector_mib"] for s in state["samples"]),
                              "minimum_available_mib": min(s["available_mib"] for s in state["samples"]),
                              "peak_buffer_mib": max(s["buffer_mib"] for s in state["samples"])}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=int, default=600)
    run(parser.parse_args().seconds)
