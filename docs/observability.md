# Bridge service logs

The VPS `147.182.158.187` runs **only Grafana Alloy**, sending maker and relayer
journald logs to [Grafana Cloud](https://zealousglider2700.grafana.net).
The final scope is deliberately logs only, following the request to keep setup
simple. Host metrics, tracing and custom dashboards are not enabled.

## Viewing failures

Ready-to-open views (last 24 hours): [Maker logs](https://zealousglider2700.grafana.net/explore?schemaVersion=1&panes=%7B%22logs%22%3A%7B%22datasource%22%3A%22grafanacloud-logs%22%2C%22queries%22%3A%5B%7B%22refId%22%3A%22A%22%2C%22expr%22%3A%22%7Bapp%3D%5C%22zecswap%5C%22%2Cenvironment%3D%5C%22testnet%5C%22%2Cservice%3D%5C%22zecswap-maker%5C%22%7D%22%2C%22queryType%22%3A%22range%22%2C%22datasource%22%3A%7B%22type%22%3A%22loki%22%2C%22uid%22%3A%22grafanacloud-logs%22%7D%2C%22editorMode%22%3A%22code%22%2C%22direction%22%3A%22backward%22%7D%5D%2C%22range%22%3A%7B%22from%22%3A%22now-24h%22%2C%22to%22%3A%22now%22%7D%7D%7D), [Relayer API logs](https://zealousglider2700.grafana.net/explore?schemaVersion=1&panes=%7B%22logs%22%3A%7B%22datasource%22%3A%22grafanacloud-logs%22%2C%22queries%22%3A%5B%7B%22refId%22%3A%22A%22%2C%22expr%22%3A%22%7Bapp%3D%5C%22zecswap%5C%22%2Cenvironment%3D%5C%22testnet%5C%22%2Cservice%3D%5C%22zecswap-relayer%5C%22%7D%22%2C%22queryType%22%3A%22range%22%2C%22datasource%22%3A%7B%22type%22%3A%22loki%22%2C%22uid%22%3A%22grafanacloud-logs%22%7D%2C%22editorMode%22%3A%22code%22%2C%22direction%22%3A%22backward%22%7D%5D%2C%22range%22%3A%7B%22from%22%3A%22now-24h%22%2C%22to%22%3A%22now%22%7D%7D%7D).

Open [Explore](https://zealousglider2700.grafana.net/explore), select
`grafanacloud-zealousglider2700-logs`, choose a time range, and run:

```logql
{app="zecswap",environment="testnet",service=~"zecswap-maker|zecswap-relayer"}
```

Add a filter for failures:

```logql
{app="zecswap",environment="testnet",service=~"zecswap-maker|zecswap-relayer"}
  |~ "(?i)error|warn|failed|panic|timeout|unavailable"
```

Watchtower activity uses `|~ "(?i)watchtower|deposit|claim|refund|opened swap"`.
RPC/Zcash failures use `|~ "(?i)rpc|zcash|lightwallet|ethereum|sync|scan"`
followed by the failure filter above. No entries can mean an idle service;
these are logs, not an uptime alert.

Public IDs are searchable as structured metadata, outside the indexed selector:

```logql
{app="zecswap",environment="testnet"} | swap_id="PUBLIC_SWAP_ID"
{app="zecswap",environment="testnet"} | transaction_hash="PUBLIC_TX_HASH"
{app="zecswap",environment="testnet"} |= "PUBLIC_TX_HASH"
```

Only `app`, `environment`, `network`, `evm_chain`, `instance`, and `service`
are emitted as indexed labels; Grafana Cloud also adds a matching `service_name`.
Current values include `testnet`, `zcash-testnet`, `sepolia`,
and `bridge-vps-147-182-158-187`. Future mainnet must use its own service readers
and explicit `mainnet` / `zcash-mainnet` / `ethereum` labels. Never relabel the
existing testnet reader as mainnet. Hashes, swap IDs and addresses must not become
indexed labels.

## Free plan and resource limits

This setup uses ordinary Cloud Logs supported by the permanent Free plan:
50 GB/month and 14-day retention at setup time. It has no dependency on trial-only
features. The account reported **Free trial**; retain/select Free when it ends.
No subscription, billing setting, paid resource or additional stack was created.
Account-wide usage includes any other services sending to the same stack.
See [Grafana pricing](https://grafana.com/pricing/) and
[trial-to-Free account information](https://grafana.com/docs/grafana-cloud/learn-and-build/get-started/set-up-your-account/).

Alloy is capped at 5% of one CPU, 128 MiB memory, zero swap, low CPU/I/O priority,
and one Go processor. Its Go memory target is 64 MiB; cgroup memory pressure starts
at 96 MiB. Logs use a 64 KiB batch with bounded retries, with no log WAL. Its four
journal cursors occupy a private **8 MiB maximum RAM-only, nonswappable filesystem**.
There is no persistent telemetry spool or local Grafana/Loki/Prometheus database.
The pinned Alloy executable itself takes about 545 MiB of disk; the download archive
was removed. Existing journal retention and bridge storage are unchanged.

A shared limit allows two lines/second (burst 100) and drops lines over 8 KiB.
This bounds a worst-case continuous stream below roughly 44 GB of log text per
31-day month; normal bridge volume is far smaller. Bursts, oversized messages,
prolonged outages and collector stops may lose telemetry. On restart the RAM-only
cursors reset: maker logs replay at most five minutes, and the usually idle relayer
replays at most three hours. Duplicates are possible.

A small systemd watchdog checks host memory and the existing lightweight maker
`/healthz` and relayer `/v1/terms` endpoints every 30 seconds. It stops only Alloy
and removes `/etc/alloy/ENABLED` if host resource pressure threatens the bridge. Immediate
thresholds are available memory below 128 MiB, collector memory above 120 MiB,
an Alloy OOM, or scratch use above 6 MiB. Two consecutive samples below 192 MiB
available memory or above 5% memory pressure also stop it. An API or RPC failure
alone does **not** stop collection: those failure logs are needed for diagnosis.
`Restart=no` prevents restart loops. Investigate before restoring the latch.

No component calls `/v1/monitor`, requests prices, creates swaps, sends transactions,
or changes Telegram notifications. Market pricing stays request-driven. Neither
bridge service needs a restart to install, stop or roll back logging.

## Configuration and credentials

Source: [`deploy/observability`](../deploy/observability/). Installed scripts live in
`/opt/zecswap-observability`; runtime config is `/etc/alloy/config.alloy`.
The pinned [Alloy 1.20.1 release](https://github.com/grafana/alloy/releases/tag/v1.20.1)
was checked against the official archive SHA-256:
`451fe650e8277d22d69cb8db50bba809f581fe78decba7fce4027ef185457be9`.

`/etc/alloy/cloud.env`, `/etc/alloy/ingestion.token`, `/etc/alloy/redact.regex`,
and `/root/grafana-cloud.json` are root-owned `0600`. Systemd `LoadCredential`
provides the unprivileged Alloy process with runtime credential copies. HTTPS
requires TLS 1.2 or newer, validates certificates and does not follow redirects.
The ingestion endpoint is `https://logs-prod-028.grafana.net/loki/api/v1/push`;
the numeric username is `1810659`.

The stack-scoped access policy is restricted to `logs:write`. The original token
shared through chat was replaced after verification; its replacement exists only
in protected server files. Delete the unused original token named
`zecswap-alloy-zecswap-alloy` in Grafana Cloud Access Policies. The API rejected
revocation because the supplied policy lacks `accesspolicies:delete`; permissions
were not expanded. Both tokens are constrained by the ingestion-only policy. Do not include credentials in commands,
Git, dashboard variables or tickets.

Redaction happens before forwarding and field extraction. It removes exact known
bridge secrets, authenticated Alchemy URLs, authorization headers, CMC/Telegram
keys, seeds/mnemonics, private and viewing keys, while preserving public hashes.
After rotating bridge secrets, run `configure.py` and restart only Alloy to refresh
its exact-secret patterns. Unknown unlabelled secrets or novel encodings cannot be
reliably recognized by a regex; avoid logging secrets at source and extend the
fixtures when adding a new format.

Alloy listens on `127.0.0.1:12345`; profiling and support bundles are disabled.
The `ZECSWAP_ALLOY_ADMIN` firewall chain limits this port to root and Alloy because
evaluated redaction configuration can contain exact secret patterns. Other ports
and existing firewall rules are unchanged. Access via a root SSH tunnel only;
do not publish the UI through nginx or the bridge tunnel.

## Validation and operation

`test_redaction.py` runs 31 synthetic fixtures through the real Alloy pipeline,
checking redaction, preserved public hashes/IDs, structured metadata, and labels.
`soak.py` temporarily sends real logs to a loopback receiver, decodes the actual
wire payloads, and checks that exact configured secrets are absent. It samples
resource/health data and reads scanned block heights through short read-only SQLite
queries without triggering scans. `verify_cloud.py` queries received logs and checks
for known-secret leakage without printing real log bodies; it needs a temporary
`logs:read` credential, which the final ingestion-only token intentionally lacks. These are on-demand
validation tools, not continuously running agents.

Configuration updates and token rotation, run as root on the VPS:

```sh
python3 /opt/zecswap-observability/configure.py --cloud-file /root/grafana-cloud.json
python3 /opt/zecswap-observability/install.py
systemctl restart alloy
```

For deliberate activation after a watchdog stop, first investigate its journal and
`/run/zecswap-alloy-guard.json`, then:

```sh
touch /etc/alloy/ENABLED
systemctl enable --now zecswap-alloy-guard.timer
systemctl enable --now alloy
```

Confirm `systemctl is-active alloy zecswap-maker zecswap-relayer`, check
`journalctl -u alloy --since '10 minutes ago'`, and verify recent Cloud logs.
Never run the local canary and production Alloy simultaneously on port 12345.

## Rollback

```sh
sh /opt/zecswap-observability/rollback.sh
```

This stops/disables Alloy and its watchdog, clears the activation latch, and removes
only its admin-port firewall rules. Its RAM buffer disappears. Bridge services,
Telegram, pricing, wallets and databases are untouched. Configuration and protected
credentials remain for diagnosis; delete only collector files if removing it fully.

The pre-install configuration backup is
`/root/zecswap-observability-backup-20261002T151641Z/`, root-only. Its archive contains
existing configuration secrets; keep it protected. It also contains the original
firewall rules and SHA-256 manifest. It is not a wallet/database backup. All 31
original configuration files were verified unchanged during setup.

## Verified on 2026-10-02

The final logs-only trial ran for 186 seconds while both Zcash wallets advanced
from height 4,434,172 to 4,434,176. Peak collector memory was 37.03 MiB, CPU averaged
0.50% of one core, RAM scratch peaked at 1.08 MiB, and at least 503 MiB host memory
remained available. Both APIs stayed healthy. An earlier ten-minute trial with
additional metrics also passed; those metrics were removed from the final setup.

Cloud queries confirmed both maker and relayer logs arrived. All 31 local fixtures
and four Cloud fixtures passed; public identifiers survived and the Cloud series
API confirmed they were not indexed. Sampled real logs contained no exact known
secrets. The temporary synthetic file source was removed. No logs were dropped
during the delivery checks. All 31 pre-existing configuration files matched their
original hashes, and neither bridge service restarted. These observations cover
normal ongoing scanning, not a full historical rescan or peak proving workload.

Protected detailed reports are `/root/zecswap-alloy-soak-report.json` and
`/root/zecswap-alloy-initial-resource-report.json`.


## Log correlation and dashboard follow-up (2026-10-02)

Live Alloy now recognizes `tx=...` in existing relayer messages, as well as
`txid`, `tx_hash`, and `transaction_hash`. These remain structured metadata,
never indexed labels. The local fixture suite includes nested tracing spans.
The exact INFO line `schemerz: Migrating everything` is filtered before the rate
limit; actual migration applications and warnings/errors remain. The flow
observer opens its viewing wallet every 30 seconds and the library prints this
line before checking already-applied migrations. It does not indicate a bridge
restart or prove that any migration changed the database. Existing Cloud entries
remain visible until retention expires; this filter affects newly forwarded logs.

Application logging changes are deployed in release `20261002-rescue-bcc362f`:

- Maker acceptance joins quote ID to swap ID and the escrow-open transaction.
- Maker watchtower/sweep and forward/reverse relayer errors retain operation and
  swap context. Instrumentation explicitly skips request bodies and private data.
- EVM submissions record their public hash before receipt waiting. Results are
  `mined`, `reverted`, or `unknown` when receipt retrieval fails. `mined` does not
  mean finality, and `unknown` does not prove the transaction failed.
- Zcash broadcasts retain their hash on errors; acceptance is separate from
  mining/confirmation. Flow observations log transaction state changes; EVM event
  observations include the public swap/hash at the configured confirmation depth.
- Background observer failures distinguish timeout, panic, worker, database,
  wallet and RPC/contract categories without emitting raw private RPC payloads.

Failures before broadcast can have a swap/quote ID but no transaction hash.
Wallet/client failures before reaching either service are not visible in server
logs. Use the swap ID across both services, then inspect transaction-specific
lines. Older logs cannot acquire context that the old binary never emitted.
On explicit approval, both services were upgraded on 2026-10-02 together with the
new Sepolia contract `0xa067d2e46f7cea71f4e4fc862b6444ecc1450afc`. Its deployed
runtime matches the build and the nonce/deadline rescue ABI is verified. The old
contract is retired. Both services report the new address and use the rebuilt
binaries. Swap history started empty; the maker root secret was rotated because
quote nonces restart. The existing ETH keys, Zcash seed, funded wallet and scan
state were retained, as were Telegram, CMC pricing and monitoring credentials.
A synthetic rejected relayer request verified that operation/swap context appears
without the secret share, signature or note. It sent no transaction.

The old configuration and consistent SQLite snapshots are protected in
`/var/backups/zecswap/20261002-retired-bd9a37`. Old application/flow databases live
in its `retired-runtime` directory, outside the service's active data directory.
Rollback now requires coordinating contract, inventory, root secret and swap
store; do not restore just an old binary/config or overwrite the current funded
wallet. Reconcile any swaps on the new contract before moving inventory back.

Validation passed: all 47 maker unit tests, a relayer rejection test that verifies
swap context while excluding private payloads, and a local Anvil transaction test
that verifies matching swap/hash on submission and mining. Checks and Clippy for
maker/relayer/chain passed with warnings denied. The deployed relayer also passed the correlated rejection check.

The separate `zapp-dashboard` repository has a deployed `/bridge/logs` page and
protected `/api/bridge/logs` server route. It reads Cloud logs only on demand,
keeps the token in Vercel server environment variables, and provides links from
individual swaps. The existing dashboard is public, so logs have separate Basic
authentication using existing dashboard credentials or dedicated `LOGS_*` values.
A dedicated stack-scoped `logs:read` token is required as a sensitive Vercel
`GRAFANA_LOGS_READ_TOKEN`; the VPS policy remains `logs:write` only. After the
owner updated the GitHub sign-in connection, a normal manual deployment succeeded
as `dpl_EPVWCWe32WwXuxqz5tipnMfHfSbJ` from dashboard source `b04b2f1`.
Both Logs routes return 401 with `private, no-store` when unauthenticated.
All nine live bridge checks are healthy with no deployment mismatch alerts.
Live Cloud reads remain unverified until the dedicated read token is supplied
and the dashboard is redeployed. See that repository's README for setup,
validation and rollback.

A post-cutover sample measured Alloy at 40.4 MiB, 537 MiB available host memory,
no memory-pressure/OOM events, and 1.07 MiB cursor storage in its bounded tmpfs.
Since the collector restart it had sent 86 entries with zero write retries or
drops. Both bridge APIs were healthy. This confirms successful ingestion writes;
an additional Cloud readback awaits read credentials.
