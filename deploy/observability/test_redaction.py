#!/usr/bin/env python3
"""Integration test against Alloy itself; synthetic inputs never leave localhost."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


def run(config, alloy):
    original = Path(config).read_text()
    pipeline = original.split("// BEGIN REDACTION PIPELINE", 1)[1].split("// END REDACTION PIPELINE", 1)[0]
    pipeline = pipeline[pipeline.index('loki.process "bridge"'):]
    pipeline = pipeline.replace("loki.write.cloud.receiver", "loki.echo.test.receiver")
    tx = "0x" + "ab" * 32
    swap = "cd" * 32
    known = "SYNTHETIC_EXACT_SECRET_9bca7612"
    fixtures = [
        ("url=https://eth-sepolia.g.alchemy.com/v2/SYNTHETIC_ALCHEMY_KEY", "SYNTHETIC_ALCHEMY_KEY"),
        (r'url=https:\/\/eth-mainnet.g.alchemy.com\/v2\/SYNTHETIC_ESCAPED_KEY', "SYNTHETIC_ESCAPED_KEY"),
        ('url=https://SYNTHETIC_USER:SYNTHETIC_PASSWORD@rpc.example.org', "SYNTHETIC_PASSWORD"),
        ('Authorization: Bearer SYNTHETIC_AUTH_TOKEN', "SYNTHETIC_AUTH_TOKEN"),
        ('{"Authorization":"Basic U1lOVEhFVElDX0JVU0lD"}', "U1lOVEhFVElDX0JVU0lD"),
        ('authorization=Bearer SYNTHETIC_BEARER_TOKEN', "SYNTHETIC_BEARER_TOKEN"),
        ('ZCASH_CMC_KEY=SYNTHETIC_CMC_KEY', "SYNTHETIC_CMC_KEY"),
        ('X-CMC_PRO_API_KEY: SYNTHETIC_CMC_HEADER', "SYNTHETIC_CMC_HEADER"),
        ('TELEGRAM_BOT_TOKEN=SYNTHETIC_TELEGRAM_KEY', "SYNTHETIC_TELEGRAM_KEY"),
        ('https://api.telegram.org/bot123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/sendMessage', '123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789'),
        ('seed="abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"', "abandon"),
        ('seed=abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about', "abandon"),
        ('MAKER_ZCASH_SEED=[12, 34, 56, 78, 90]', "12, 34, 56"),
        ('MAKER_PRIVATE_KEY=0x' + '12' * 32, '12' * 32),
        ('privateKey="' + '23' * 32 + '"', '23' * 32),
        ('viewing_key=uviewtest1syntheticviewingkey123', 'uviewtest1syntheticviewingkey123'),
        ('bare uview1syntheticviewingkey789', 'uview1syntheticviewingkey789'),
        ('private key: SYNTHETIC_PRIVATE_KEY', 'SYNTHETIC_PRIVATE_KEY'),
        ('MAKER_ROOT_SECRET=SYNTHETIC_ROOT_SECRET', 'SYNTHETIC_ROOT_SECRET'),
        ('{"cmc_key": "SYNTHETIC_JSON_KEY", "viewingKey": "SYNTHETIC_VIEWING_KEY"}', 'SYNTHETIC_JSON_KEY'),
        ('message=' + known, known),
        ('rpc failed: url=https://eth-sepolia.g.alchemy.com/v2/SYNTHETIC_NESTED_KEY?api_key=SYNTHETIC_QUERY_KEY', 'SYNTHETIC_NESTED_KEY'),
        ('seed=1234567890abcdef1234567890abcdef', '1234567890abcdef1234567890abcdef'),
        ('{"seed": "SYNTHETIC_QUOTED_SEED with spaces"}', 'SYNTHETIC_QUOTED_SEED'),
        ('TELEGRAM_CHAT_ID=-1001234567890', '-1001234567890'),
        ('ISSUER_MONITOR_TOKEN=SYNTHETIC_ISSUER_MONITOR_TOKEN', 'SYNTHETIC_ISSUER_MONITOR_TOKEN'),
        ('RELAYER_MONITOR_TOKEN=SYNTHETIC_RELAYER_MONITOR_TOKEN', 'SYNTHETIC_RELAYER_MONITOR_TOKEN'),
    ]
    with tempfile.TemporaryDirectory(prefix="alloy-redaction-", dir="/run") as temp:
        d = Path(temp)
        (d / "redact.regex").write_text("(" + re.escape(known) + ")")
        lines = [f"case={i:03} {line} txid={tx} id={swap}" for i, (line, _) in enumerate(fixtures)]
        lines.append(f"case=public INFO watchtower action=observe txid={tx} id={swap}")
        lines.extend([
            f"case=relayer INFO claimed id={swap} tx={tx}",
            f'case=span WARN accept{{quote_id=0x{"ef" * 32} swap_id={swap}}}:broadcast{{transaction_hash={tx}}}: error=rpc_failed seed="{known}"',
            f"case=migration INFO schemerz: Applying migration txid={tx} id={swap}",
            f"case=migration_error WARN schemerz: migration failed txid={tx} id={swap}",
            "case=idle 2026-10-02T16:23:20Z  INFO schemerz: Migrating everything",
        ])
        (d / "fixtures.log").write_text("\n".join(lines) + "\n")
        source = f'''logging {{
 level = "info"
 format = "json"
}}
local.file "known_secrets" {{
 filename = sys.env("CREDENTIALS_DIRECTORY") + "/redact.regex"
 is_secret = true
}}
loki.source.file "fixtures" {{
 targets = [{{__path__ = "{d}/fixtures.log", app = "zecswap", environment = "testnet", service = "synthetic"}}]
 forward_to = [loki.process.bridge.receiver]
}}
loki.echo "test" {{}}
'''
        (d / "config.alloy").write_text(source + pipeline)
        env = os.environ | {"CREDENTIALS_DIRECTORY": str(d), "GOMAXPROCS": "1", "GOMEMLIMIT": "96MiB"}
        subprocess.run([alloy, "validate", str(d / "config.alloy")], env=env, check=True, capture_output=True)
        with (d / "output").open("w") as out:
            p = subprocess.Popen([alloy, "run", "--disable-reporting", "--server.http.listen-addr=127.0.0.1:12346",
                                  "--storage.path=" + str(d / "state"), str(d / "config.alloy")], env=env, stdout=out, stderr=out)
            try:
                for _ in range(150):
                    if p.poll() is not None or "case=migration_error" in (d / "output").read_text():
                        break
                    time.sleep(0.2)
            finally:
                p.terminate()
                p.wait(timeout=10)
        output = (d / "output").read_text()
        assert "case=public" in output, "Alloy did not process fixtures: " + output[:4000]
        for i, (_, secret) in enumerate(fixtures):
            line = next((s for s in output.splitlines() if f"case={i:03}" in s), "")
            assert line, f"Missing fixture {i}"
            assert secret not in line, f"Secret leaked in fixture {i} (value withheld)"
            assert tx in line and swap in line, f"Public identifiers lost in fixture {i}"
        assert "SYNTHETIC_VIEWING_KEY" not in output
        assert "SYNTHETIC_QUERY_KEY" not in output
        assert "case=idle" not in output, "Routine wallet check was not filtered"
        for case in ["relayer", "span", "migration", "migration_error"]:
            assert f"case={case} " in output, f"Missing operational fixture {case}"
        # IDs must never become indexed stream labels.
        for line in output.splitlines():
            if "case=" in line:
                parsed = json.loads(line)
                labels = parsed.get("labels", {})
                assert "swap_id" not in labels and "transaction_hash" not in labels
                metadata = json.loads(parsed["structured_metadata"])
                assert metadata.get("swap_id") == swap and metadata.get("transaction_hash") == tx, (parsed["entry"].split()[0], metadata)
        assert known not in output, "A secret leaked from a nested tracing span"
        print(json.dumps({"passed": len(fixtures) + 6, "public_hashes_preserved": True, "alloy_version": "1.20.1"}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", default="/etc/alloy/config.alloy")
    parser.add_argument("--alloy", default="/usr/local/bin/alloy")
    args = parser.parse_args()
    run(args.config, args.alloy)
