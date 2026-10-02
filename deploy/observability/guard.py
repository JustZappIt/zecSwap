#!/usr/bin/env python3
"""Availability watchdog; never changes or restarts bridge services."""
import json
from pathlib import Path
import subprocess
import time
import urllib.request


def health(url):
    started = time.monotonic()
    try:
        with urllib.request.urlopen(url, timeout=3) as response:
            return response.status in (200, 204), round(time.monotonic() - started, 3)
    except Exception:
        return False, round(time.monotonic() - started, 3)


def sample(unit="alloy.service"):
    memory = dict(line.split(":", 1) for line in Path("/proc/meminfo").read_text().splitlines())
    cg = Path("/sys/fs/cgroup/system.slice") / unit
    def value(name):
        p = cg / name
        return int(p.read_text()) if p.exists() else 0
    def fields(name):
        p = cg / name
        return dict(line.split() for line in p.read_text().splitlines()) if p.exists() else {}
    psi = Path("/proc/pressure/memory").read_text().splitlines()[0].split()
    maker, maker_seconds = health("http://127.0.0.1:8787/healthz")
    relayer, relayer_seconds = health("http://127.0.0.1:8788/v1/terms")
    return {"time": int(time.time()), "available_mib": int(memory["MemAvailable"].split()[0]) / 1024,
            "collector_mib": value("memory.current") / 1048576, "memory_events": fields("memory.events"),
            "cpu_usec": int(fields("cpu.stat").get("usage_usec", 0)),
            "memory_psi_avg10": float(dict(s.split("=") for s in psi[1:])["avg10"]),
            "maker_ok": maker, "maker_seconds": maker_seconds, "relayer_ok": relayer, "relayer_seconds": relayer_seconds}


if __name__ == "__main__":
    if subprocess.run(["systemctl", "is-active", "--quiet", "alloy.service"]).returncode:
        raise SystemExit(0)
    record = sample()
    state = Path("/run/zecswap-alloy-guard.json")
    previous = json.loads(state.read_text()) if state.exists() else {}
    # An RPC outage or failed bridge service is precisely when logs are needed.
    # Stop for resource pressure, never for API failure alone.
    bad = record["available_mib"] < 192 or record["memory_psi_avg10"] > 5
    record["bad_samples"] = previous.get("bad_samples", 0) + 1 if bad else 0
    urgent = record["available_mib"] < 128 or record["collector_mib"] > 120
    if int(record["memory_events"].get("oom", 0)) > 0:
        urgent = True
    pid = subprocess.check_output(["systemctl", "show", "alloy", "-p", "MainPID", "--value"], text=True).strip()
    storage = Path(f"/proc/{pid}/root/run/alloy")
    if storage.exists():
        import os
        st = os.statvfs(storage)
        record["buffer_mib"] = (st.f_blocks - st.f_bfree) * st.f_frsize / 1048576
        urgent |= record["buffer_mib"] > 6
    state.write_text(json.dumps(record))
    if urgent or record["bad_samples"] >= 2:
        Path("/etc/alloy/ENABLED").unlink(missing_ok=True)
        subprocess.run(["systemctl", "stop", "alloy.service"], check=True)
        print("Alloy stopped and latched off to protect bridge availability: " + json.dumps(record))
