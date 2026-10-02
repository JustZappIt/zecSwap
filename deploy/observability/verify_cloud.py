#!/usr/bin/env python3
"""Query Cloud without printing credentials or real bridge log bodies."""
import argparse
import base64
import json
from pathlib import Path
import re
import time
import urllib.parse
import urllib.request


def query(expression, start=None):
    data = json.loads(Path("/root/grafana-cloud.json").read_text())
    url = data["logs_url"].removesuffix("/push") + "/query_range?"
    params = {"query": expression, "limit": "1000", "start": str(start or int((time.time() - 3 * 3600) * 1e9)), "end": str(int(time.time() * 1e9))}
    auth = base64.b64encode((data["logs_username"] + ":" + data["ingestion_token"]).encode()).decode()
    request = urllib.request.Request(url + urllib.parse.urlencode(params), headers={"Authorization": "Basic " + auth})
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.load(response)["data"]["result"]


def indexed_series():
    data = json.loads(Path("/root/grafana-cloud.json").read_text())
    params = {"match[]": '{app="zecswap",environment="testnet"}', "start": str(int((time.time() - 3 * 3600) * 1e9))}
    auth = base64.b64encode((data["logs_username"] + ":" + data["ingestion_token"]).encode()).decode()
    url = data["logs_url"].removesuffix("/push") + "/series?" + urllib.parse.urlencode(params)
    with urllib.request.urlopen(urllib.request.Request(url, headers={"Authorization": "Basic " + auth}), timeout=20) as response:
        return json.load(response)["data"]


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--marker")
    args = parser.parse_args()
    expression = '{app="zecswap",environment="testnet"}'
    if args.marker:
        assert re.fullmatch(r"[a-zA-Z0-9_-]+", args.marker)
        expression += ' |= "' + args.marker + '"'
    result = query(expression)
    secrets = re.compile(Path("/etc/alloy/redact.regex").read_text())
    counts = {}
    redactions = 0
    # query_range merges structured metadata into returned labels. The series
    # endpoint reports the actual indexed stream labels, which is what matters.
    for series in indexed_series():
        assert "swap_id" not in series and "transaction_hash" not in series
    for stream in result:
        labels = stream["stream"]
        for entry in stream["values"]:
            assert not secrets.search(entry[1]), "Known secret appeared in cloud (value withheld)"
            assert not re.search(r"SYNTHETIC_CLOUD_(?:ALCHEMY|AUTH|CMC|TELEGRAM|SEED|PRIVATE|VIEWING)_SECRET", entry[1]), "Synthetic secret leaked"
            counts[labels.get("service", "unknown")] = counts.get(labels.get("service", "unknown"), 0) + 1
            redactions += "[REDACTED" in entry[1]
            if args.marker:
                assert "0x" + "ab" * 32 in entry[1] and "cd" * 32 in entry[1], "Public identifiers missing"
    print(json.dumps({"entries_by_service": counts, "redacted_entries": redactions, "known_secret_leaks": 0}))
    assert counts, "No matching logs arrived"
