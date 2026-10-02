# Public testnet services

The maker and relayer run on the DigitalOcean Droplet at `147.182.158.187`. A Cloudflare Worker
at `https://zecswap-testnet.pepeman931.workers.dev` forwards requests through a private Workers
VPC Service and a named Cloudflare Tunnel. The phone needs neither USB forwarding nor a
running development Mac. The databases remain on the Droplet's persistent disk.

| API | Base URL |
| --- | --- |
| Maker | `https://zecswap-testnet.pepeman931.workers.dev/maker` |
| Relayer | `https://zecswap-testnet.pepeman931.workers.dev/relayer` |

Both services are on the same host for this testnet deployment, under separate Unix users
and signing keys. This does not provide independent operators: the production relayer must
remain independent of the maker, as the protocol requires.

The reverse-capable Sepolia contract is deployed at
[`0xbd9a37f47a988aefc4d80395727f41feb698e225`](https://sepolia.etherscan.io/address/0xbd9a37f47a988aefc4d80395727f41feb698e225).
[sepolia-reverse.json](../deployments/sepolia-reverse.json) records the transaction, block,
test token, Railgun proxy and lock duration. Its runtime code was checked against the build,
and its lock, Railgun proxy and reverse-funding getter were checked on-chain. Both hosted
services use this deployment. The existing local phone-test services still use the prior
contract; switching Android requires updating its contract and endpoints together.

Cloudflare Workers cannot run these binaries directly. Cloudflare Containers currently have
[ephemeral disks](https://developers.cloudflare.com/containers/faq/): placing the maker's
SQLite databases there would lose swap recovery state on restart. A Cloudflare-only deployment
requires a durable-storage design before it can hold funded swaps. A persistent host behind
Tunnel is compatible with the current implementation. For independent operation, that host
must remain online when the development Mac sleeps or shuts down.

## Deployment sequence

1. Deploy the updated `contracts/script/Deploy.s.sol` on the chosen EVM testnet with the
   correct `RAILGUN` proxy. Record the chain ID, contract, token, deployment transaction and
   block. Existing deployed contracts cannot gain reverse methods.
2. Configure maker and independent relayer for that exact deployment. Use a separate data
   directory for a new maker deployment; keep existing services and their pending swaps
   running until settled. Enable `[reverse]` using the example maker config and supply
   `MAKER_ZCASH_SEED` alongside the existing maker secrets through the process environment.
3. Run `zecswap-maker --config <maker-config> zec-inventory` to obtain the seed-derived ZEC
   inventory address and balance. Fund the test inventory and the services' EVM gas accounts.
   Run `zecswap-maker --config <maker-config> serve` and
   `zecswap-relayer --config <relayer-config>` under host process supervision.
4. Configure a [Workers VPC Service](https://developers.cloudflare.com/workers-vpc/get-started/)
   for a remotely managed tunnel to the host. `deploy/nginx.conf` routes `/maker/` and
   `/relayer/` to loopback listeners. `deploy/worker/` supplies the stable `workers.dev`
   hostname; a custom domain is optional. Keep tunnel credentials and service secrets
   outside version control.
5. Validate the public endpoints below before updating the app's pinned deployment.

## Operating the deployment

`scripts/build-server.sh` cross-compiles the Linux release binaries from macOS using the
pinned Rust toolchain, cargo-zigbuild and Zig. It needs `uv` and `rustup`. Upload the binaries
from `target/x86_64-unknown-linux-gnu/release/` to a new `/opt/zecswap/releases/<release>/`
directory, then switch `/opt/zecswap/current` and restart the services. Retain the previous
release for rollback; do not replace or roll back wallet databases during a binary rollback.

The systemd units are in `deploy/systemd/`. The deployed services are `zecswap-maker`,
`zecswap-relayer`, and `zecswap-tunnel`, all enabled at boot. The maker uses one proving
thread and two async workers so proving does not occupy the only API worker. Its state lives
in `/var/lib/zecswap-maker`; configs and root-readable environment
files live in `/etc/zecswap-maker` and `/etc/zecswap-relayer`. The tunnel uses a systemd
credential loaded from `/etc/zecswap-tunnel/token`. API listeners are loopback-only; the
firewall permits inbound SSH. The host has 1 GB RAM, a 25 GB disk and a 1 GB swap file.

The maker config must be readable by its service account: root ownership, group
`zecswap-maker`, mode `0640`. Preserve that group and mode when replacing the file
atomically. Environment files can remain root-owned `0600`, since systemd reads them
before switching to the service account.

From `deploy/worker/`, run `npm ci`, `npm run types`, `npm run check`, `npm test`, then
`npx wrangler deploy --dry-run` before `npm run deploy`. The VPC binding is pinned in
`wrangler.jsonc`. The gateway streams request bodies unchanged, disables caching and does
not retry financial requests. The nginx gateway limits traffic to five requests per second
with a burst of twenty, shared across clients through the tunnel.

Keep the maker seed, root secret, and both databases together in protected backups. A disk
on the VPS is persistent storage, not an off-host backup. Restoring an old snapshot also
requires reconciling newer on-chain swaps before admitting new quotes.

Additional API abuse protection is deferred. The existing forward acceptance endpoint spends
maker gas before the user's ZEC deposit. Reverse maker payments require confirmed escrow, but
relayer sponsorship has no token/maker allowlist or funding-confirmation admission policy.
The shared nginx rate limit is not sufficient protection against deliberate gas griefing or
abandoned reservations. The live smoke tests establish protocol functionality, not production
abuse resistance. Keep the existing forward ordering: depositing ZEC before escrow exists
would remove its contract-backed recovery path.

## Public verification

### Telegram bridge alerts

Set `TELEGRAM_BOT_TOKEN` and a numeric `TELEGRAM_CHAT_ID` in the maker's protected
process environment. Both must be supplied together; omitting both disables alerts.
The hosted testnet reuses the onramp gas monitor's existing bot and private destination.
It only calls Telegram `sendMessage`; it does not change that bot's webhook or consume
updates. Keep these credentials out of Git, dashboard variables, browser responses and
command arguments.

On the host, use a root-owned `0600` `/etc/zecswap-maker/telegram.env` and a systemd
drop-in `/etc/systemd/system/zecswap-maker.service.d/telegram.conf`:

```ini
[Service]
EnvironmentFile=/etc/zecswap-maker/telegram.env
```

Reload systemd and restart the maker after configuring the environment. Under the
same service environment and account, run
`zecswap-maker --config /etc/zecswap-maker/config.toml telegram-test` to send one
clearly labeled setup message without creating a financial swap. Successful output
means Telegram acknowledged delivery, not that the recipient has read the message.

Accepted bridges and terminal outcomes are queued transactionally with their SQLite
state changes. Messages include the Zcash network, EVM chain/contract/maker, direction,
exact locked USDC/ZEC amounts, swap ID, deadlines, any recorded ZEC deposit/recovery
transaction IDs, and the matching dashboard link. Refunds and expired swaps are
distinct from successful settlement; an observed claim with a pending escrow payout
is explicitly labeled. User-side private ZEC recovery is not observable by the maker.
No price-feed calls are made by the notification worker.
Deposit alerts also report observed user ZEC funds or confirmed user USDC escrow funding.
Rebroadcasting a recorded ZEC transaction may return a backend duplicate error while
it is already in the mempool or mined. The adapter checks `GetTransaction` for the
same transaction ID and exact serialized bytes before treating such a rejection as
successful submission. A transaction known only on a noncanonical fork, a different
transaction, an unavailable lookup, or a five-second lookup timeout retains the
original error. This does not replace wallet confirmation or escrow readiness checks.

Delivery runs independently of the watchtower with a ten-second request timeout.
The durable queue retries outages with backoff, respects Telegram rate limits, and
keeps event IDs after successful delivery so restarts do not replay them. A message
can be repeated if Telegram accepts it but its acknowledgement is lost. Existing
settled history is never replayed; active swaps are enrolled when alerts are enabled.
Swap and service errors notify once per failure episode, clearing after a successful
pass. Protect `maker.sqlite` backups since they now also contain public swap alert text.

`GET /v1/monitor` includes `notifications`: enabled state, pending count, oldest
pending time, last acknowledged delivery time, and a sanitized delivery error.
It never includes the bot token or destination ID. The dashboard displays this as
the Telegram alerts health check. Mainnet uses the same code: deploy its own maker
with separate configuration/data and supply the bot/destination environment pair.
Messages and deduplication IDs include the deployment and network.

Ethereum transaction references are indexed from the configured settlement contract's
confirmed events, including transactions sent by the app or relayer. A separate
read-only task scans ten-block RPC windows without locking the wallet or fetching
prices. Recent blocks are scanned first; historical swaps are backfilled from their
earliest accepted quote. SQLite stores the cursor and public event metadata under
the network/chain/contract/maker scope. Each pass reconciles the latest twelve blocks
to remove orphaned events after a shallow reorganization; deeper reorgs require an
operator rescan. Confirmation depth follows `reverse.evm_confirmations`, or two when
reverse swaps are disabled. Confirmed does not mean finalized.

Telegram messages label the swap ID as a bridge reference, and include actual
transaction hashes with Etherscan and Railscan links on Ethereum mainnet/Sepolia.
New funding, claim, payout, rescue and refund events enqueue a link update; readiness
and lock events are visible in the dashboard without extra Telegram updates.
Historical backfill never replays old events. Transaction alert deduplication and
cursor advancement are atomic. `/v1/monitor` exports `transactions` indexing status
and each swap's `evmTransactions` public hash/block/event list, never log payloads,
revealed shares, RPC credentials or Telegram configuration.

### Dashboard monitoring

Live USD pricing is enabled by `[pricing.market]` in the maker configuration:

```toml
[pricing.market]
token_decimals = 6
refresh_seconds = 60
max_age_seconds = 300
```

Set `ZCASH_CMC_KEY` only in the maker's protected environment file. It is never a
dashboard variable or part of a browser response. Quote and monitoring requests fetch
CoinMarketCap's ZEC (ID 1437) and USDC (ID 3408) USD prices together, using a shared
60-second cache and a single in-flight request. There is no cron or background polling.
ZEC/USDC is ZEC/USD divided by USDC/USD, converted to six-decimal token units with decimal
arithmetic; the existing spread and zatoshi rounding apply in both swap directions.
Both asset timestamps must be within the configured maximum age. A provider failure can
reuse the last valid price only within that limit; without one, new quotes return 503
with `unavailable`. Provider retries are limited to once per ten seconds after failures.
The old fixed price is ignored whenever market pricing is enabled. Issued quotes retain
their exact stored amounts until expiry; settlement and watchtower recovery continue
independently of the price feed. Omitting `[pricing.market]` retains fixed pricing for
local tests. Production and mainnet should explicitly enable market pricing.

`GET /maker/v1/monitor` is a read-only operations export for `zapp-dashboard`. It is
disabled unless `MAKER_MONITOR_TOKEN` is set (at least 32 characters), and requires
`Authorization: Bearer <token>`. Use the same value as `BRIDGE_TESTNET_MONITOR_TOKEN`
in the dashboard's server environment. No seed, spending share, viewing key, user
authorization, or wallet account identifier is returned.

The export reports contract and wallet USDC, maker ETH, shielded ZEC total/spendable/
reserved/available inventory, quote and swap counts, watchtower readiness, sync recency,
errors since restart, pricing/timing policy, and up to 50 swaps with active records first.
The `pricing` object reports the same prices used for quotes, provider timestamps,
cache/freshness policy and sanitized refresh errors. USD inventory values and displayed
maker buy/sell rates use these readings, rather than another independent price feed.
Swap details include observed contract states, deposit/sweep transaction IDs, wallet funds,
and claim/refund deadlines. Settled counts include expired and refunded swaps; they are
not successful-trade counts. Per-swap errors are from the last completed watchtower pass;
the counter resets when the maker restarts. Detailed errors remain in the maker logs.

The endpoint never syncs the wallet, generates proofs, sends transactions, or writes
inventory. RPC observations have bounded concurrency and a six-second total time budget;
failed observations return unknown fields. A busy wallet uses the last captured inventory
reading and sets `walletBusy`; individual wallet readings remain unknown while busy.
The dashboard validates the deployment and token precision before formatting USDC.
The pinned test token has no optional `decimals()` metadata and uses explicitly configured
six-decimal USDC units; mainnet has no default token precision.
Mainnet uses its own dashboard environment variables and monitoring token.

- Maker `GET /v1/info`: exact expected chain ID, contract, token, Zcash network and
  `reverseEnabled: true`.
- Maker `GET /healthz`: 204 after both an EVM pass and a successful Zcash sync. Either
  becoming older than three tick intervals returns 503 and closes quote/accept admission.
  EVM deadline processing continues independently while Zcash is stalled or proving.
- Relayer `GET /v1/terms`: same chain ID and contract, with an independent relayer address.
- Maker reverse quote: valid typed terms and sufficient funded ZEC inventory. Check all
  deadlines and the maker proof before accepting.
- A complete live reverse swap: atomic Railgun unshield and escrow funding, confirmed maker
  ZEC deposit, independently verified readiness, maker claim and final ZEC sweep. Repeat
  with service restart and cancellation to verify recovery through the public endpoints.

Keep API caching disabled. A returned HTTP transaction hash means submitted, not confirmed;
after a timeout, reconcile chain state before retrying any financial action. The app must
independently verify escrow and ZEC receipt as described in [reverse-flow.md](reverse-flow.md).

Forward settlement requires `evm_confirmations` (top-level maker setting, default 12).
Reverse settlement continues to use `reverse.evm_confirmations`. Zcash sweeps require
the configured external-deposit depth (`confirmations`, default 10), measured from the
fully scanned height. Lightwalletd response bodies have a 30-second idle deadline and a
120-second total deadline, including streaming responses after headers arrive.

Keep both maker and wallet databases when upgrading or restarting. Settlement retains
accounts, birthdays, viewing keys, transaction history and sweep IDs; completed swaps
are revisited for reorg recovery. The maker database automatically adds persisted
cancellation intent, preventing a rolled-back refund lock from making a swap ready again.
Retained history grows over time and is not automatically pruned. Accounts already deleted
by an older maker are not reconstructed by this upgrade.

### Protocol smoke test

On September 28, 2026, the public gateway completed a one-test-USDC reverse swap through
confirmed ZEC receipt, including maker restarts after funding and after the ZEC deposit.
A separate cancellation completed its committed Railgun refund payout. Public transaction
evidence is in [sepolia-reverse-smoke.json](../deployments/sepolia-reverse-smoke.json).
Both tests funded escrow with public test tokens; a real Railgun unshield-to-escrow transaction
and the Android application flow remain to be validated.

`crates/zecswap-e2e/examples/reverse_deployment.rs` exercises the deployed protocol with
public test-token funding. It does not build a Railgun unshield transaction. Build with
`cargo build --profile test -p zecswap-e2e --example reverse_deployment` so proving dependencies
are optimized. Set `ETH_SEPOLIA_RPC_URL`, `ZECSWAP_MAKER_URL`, `ZECSWAP_RELAYER_URL`, and a
fresh private `REVERSE_TEST_DIR`. Run `target/debug/examples/reverse_deployment prepare`.
It verifies the pinned deployment, accepts a one-test-USDC quote, imports the deposit
account and saves the approval/open calldata in `funding-calls.json`.

Fund those exact calls with test tokens on Sepolia before the funding deadline. Keep each
signed transaction and its hash before broadcasting, and reconcile receipts on interruption.
Set `REVERSE_TEST_DESTINATION` to a shielded Zcash testnet address, then run the example with
`receive` to independently confirm the deposit, authorize readiness and sweep the received
ZEC. Resume with the same directory after interruption. Use `refund` instead of `receive`
on a separate funded test to exercise cancellation and the committed private refund payout.
Preserve the test seed and wallet: they control the ZEC and Railgun refund note.

### Full bridge flow observation

The monitor exports the complete Ethereum event history and `zcashFlow` deposit/spend
observations for each visible swap. A separate `flow-wallet.sqlite` under the maker data
directory imports only joint-account full viewing keys, including completed swaps, and
scans their history. It owns no spending keys and cannot sign or broadcast transactions.
It runs on a blocking worker separate from settlement, uses the configured lightwalletd,
and never refreshes market prices. Back up this private database alongside `maker.sqlite`;
its public export contains only transaction hashes, heights, confirmations and swap amounts.

The observer refreshes up to 500 swaps per pass, prioritizing active swaps. Confirmation
counts use fully scanned wallet height, exclude change from deposits, and require the
configured deposit confirmation policy. The UI rejects stale observations. Claims/refunds
and final ZEC escrow spends are separate steps; an Ethereum claim alone does not establish
that the user's recovery completed. Shielded recipient and amount checks come from the
viewing wallet, while ZecBlock links show public transaction inclusion.

Zcash links use `https://testnet.zecblock.com/tx/` on testnet and
`https://zecblock.com/tx/` on mainnet. Receipt logs determine Railgun participation: those
transactions link to Railscan and Etherscan; public-only transactions link to Etherscan.
Old history is backfilled without Telegram replay. Newly confirmed deposits and escrow
spends enter the existing durable Telegram queue with explorer links. No additional
credentials or price cron are needed.
