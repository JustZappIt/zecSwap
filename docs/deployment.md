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

### Dashboard monitoring

`GET /maker/v1/monitor` is a read-only operations export for `zapp-dashboard`. It is
disabled unless `MAKER_MONITOR_TOKEN` is set (at least 32 characters), and requires
`Authorization: Bearer <token>`. Use the same value as `BRIDGE_TESTNET_MONITOR_TOKEN`
in the dashboard's server environment. No seed, spending share, viewing key, user
authorization, or wallet account identifier is returned.

The export reports contract and wallet USDC, maker ETH, shielded ZEC total/spendable/
reserved/available inventory, quote and swap counts, watchtower readiness, sync recency,
errors since restart, pricing/timing policy, and up to 50 swaps with active records first.
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
- Maker `GET /healthz`: 204 after the watchtower completes its first pass. Stale or stalled
  watchtower returns 503. Readiness does not establish upstream availability or inventory.
- Relayer `GET /v1/terms`: same chain ID and contract, with an independent relayer address.
- Maker reverse quote: valid typed terms and sufficient funded ZEC inventory. Check all
  deadlines and the maker proof before accepting.
- A complete live reverse swap: atomic Railgun unshield and escrow funding, confirmed maker
  ZEC deposit, independently verified readiness, maker claim and final ZEC sweep. Repeat
  with service restart and cancellation to verify recovery through the public endpoints.

Keep API caching disabled. A returned HTTP transaction hash means submitted, not confirmed;
after a timeout, reconcile chain state before retrying any financial action. The app must
independently verify escrow and ZEC receipt as described in [reverse-flow.md](reverse-flow.md).

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
