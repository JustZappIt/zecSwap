# Public testnet services

The maker, relayer and token issuer run on the DigitalOcean Droplet at `147.182.158.187`. A Cloudflare Worker
at `https://zecswap-testnet.pepeman931.workers.dev` forwards requests through a private Workers
VPC Service and a named Cloudflare Tunnel. The phone needs neither USB forwarding nor a
running development Mac. The databases remain on the Droplet's persistent disk.

| API | Base URL |
| --- | --- |
| Maker | `https://zecswap-testnet.pepeman931.workers.dev/maker` |
| Relayer | `https://zecswap-testnet.pepeman931.workers.dev/relayer` |
| Token issuer | `https://zecswap-testnet.pepeman931.workers.dev/issuer` |

Both services are on the same host for this testnet deployment, under separate Unix users
and signing keys. This does not provide independent operators: the production relayer must
remain independent of the maker, as the protocol requires.

Both hosted services use the Sepolia contract
[`0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c`](https://sepolia.etherscan.io/address/0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c),
which stores only a hash of each swap's terms ([October 7](#october-7-terms-hash-contract)).
[sepolia-terms-hash.json](../deployments/sepolia-terms-hash.json) records the transaction,
block, test token, Railgun proxy, lock duration and the services' release. Since 19:02 UTC that
day the maker takes Privacy Pass tokens ([October 7 tokens](#october-7-tokens)): Android builds
that pin this contract and both token keys can swap; earlier builds cannot. Since 04:45 UTC on
October 8 the issuer gives tokens only to installs whose key a locked phone's secure hardware
attests, and the gateway gives each kind of request its own allowance
([October 8](#october-8-attestation-and-gateway-allowances)).

Cloudflare Workers cannot run these binaries directly. Cloudflare Containers currently have
[ephemeral disks](https://developers.cloudflare.com/containers/faq/): placing the maker's
SQLite databases there would lose swap recovery state on restart. A Cloudflare-only deployment
requires a durable-storage design before it can hold funded swaps. A persistent host behind
Tunnel is compatible with the current implementation. For independent operation, that host
must remain online when the development Mac sleeps or shuts down.

## October 2, 2026 replacement

The current Sepolia contract includes single-use, expiring rescue authorizations.
It replaces `0xbd9a37f47a988aefc4d80395727f41feb698e225`, whose available maker
inventory was withdrawn and deposited into the new contract. The old deployment
manifest and protocol smoke evidence are archived under `deployments/retired/`;
its on-chain bytecode still exists but hosted services no longer target it.

At the replacement cutover, both VPS binaries were from source `bcc362f9a820d1c97a90af67e7c858a156f56acf`, in
`/opt/zecswap/releases/20261002-rescue-bcc362f`. The code is merged into `main`.
At cutover there were no unsettled swaps. The new store started with zero swaps,
75.991968 test USDC inventory, and 0.16595541 spendable testnet ZEC retained in the
existing wallet. Maker/relayer ETH accounts and the existing test token are reused.
The maker root secret was rotated when resetting quote history, to avoid reusing
previously revealed per-swap secrets. Wallet seed and scan state are unchanged.

Old state/configuration is archived at
`/var/backups/zecswap/20261002-retired-bd9a37` (root-only). Never remove the funded
wallet or replace it with an old snapshot during a rollback. Returning to the old
contract would require reconciling new swaps, moving available inventory, and
restoring matching application state and maker root; a symlink-only rollback is
not sufficient across this contract migration.

The 53 Railgun/reverse contract tests passed, and runtime bytecode, lock duration,
Railgun proxy and rescue nonce reads were verified on the new contract. Maker and
relayer report the new contract and healthy local APIs. The September smoke test
below belongs to the retired contract; a new full live swap has not yet been run.

The dashboard's Production `BRIDGE_TESTNET_CONTRACT` now selects this address.
The existing approved Vercel release was rebuilt with that setting as deployment
`dpl_6sL3Tm3dzQm9W66GAd4BUNFgeStK`. A public dashboard API check at
2026-10-02 17:28 UTC returned all nine checks healthy, no alerts, and zero swaps.
After the owner updated the GitHub sign-in connection, a normal manual deployment
published dashboard source `b04b2f1` as `dpl_EPVWCWe32WwXuxqz5tipnMfHfSbJ`,
including the new Logs page. All nine live bridge checks remained healthy with
no alerts. Logs routes reject unauthenticated requests; Cloud queries still need
a dedicated Grafana read token in Vercel and a redeployment.

The three changed Android files pass targeted ktlint. The targeted session test
could not compile because of existing chat dependency errors involving
`replyToContentType`; no new APK was built or installed.

## October 2 initial funding sponsorship

At 18:48 UTC, relayer source `e831bda0c9423f8d5cfcb68989d8df2f0d7d13d1` was deployed in
`/opt/zecswap/releases/20261002-funding-e831bda`, which was `/opt/zecswap/current` until the
[October 6 NU7 maker upgrade](#october-6-nu7-maker-upgrade).
The release retains the exact previous maker binary; only the relayer was restarted.
The maker remained running with its existing state, and both services retained their keys.
[sepolia-relayer-funding.json](../deployments/retired/sepolia-relayer-funding.json) records the
binary checksum, configuration and public API verification.

`POST /relayer/v1/reverse/fund` is enabled on the public testnet gateway.
`GET /relayer/v1/terms` advertises `reverseFunding` with V2 Relay Adapt
`0x7e3d929ebd5bdc84d02bd3205c777578f33a214d`, the existing test token and maker, a
4,000,000 gas limit, and a 20-gwei gas-price ceiling. The existing independent relayer
wallet pays Sepolia ETH. Payout fees are unchanged. The adapter's `railgun()` was
verified against the configured settlement proxy at startup.

Public checks verified capability discovery, rejection of malformed calldata, wrong
chain, native value and unknown fields, method restrictions, and both request-size
boundaries. A 34-KiB proof-shaped request reached application validation through the new
nginx route; a body over 132 KiB was rejected. An Android HTTP-client user agent also
reached the API. Maker info still identifies the same deployment, and `/maker/healthz`
returns 204. These probes submit no transaction. A real Railgun proof and funded Android
swap remain unverified; follow the [funding API handoff](reverse-flow.md#sponsored-initial-funding-sepolia).

Rollback files are in `/var/backups/zecswap/20261002-funding-e831bda` (root-only):
`relayer-config.toml`, `nginx.conf`, and `previous-current`. Restore their original owners
and permissions (relayer config is `root:zecswap-relayer`, `0640`), restore the previous
release symlink, test/reload nginx, and restart only the relayer. Never restore or roll
back the maker wallet databases during this binary/configuration rollback.

## October 6 NU7 maker upgrade

Zcash testnet activated NU7 at block 4,465,026 (2026-10-04 18:21 UTC, consensus branch ID
`0x77190AD9`). The maker built from `bcc362f` used zcash_protocol 0.10.6, which knows NU6.3
as the newest upgrade, so lightwalletd rejected every testnet transaction it built with
"transaction uses an incorrect consensus branch id". Reverse swap `0x73e8f227…` never
received its ZEC deposit, and its user refunded through the escrow.

At 04:27 UTC the maker binary was replaced by a build of source
`bb7fb0a0ac2112b6cf735708311f33143f0cc035` (#3 and #5), in
`/opt/zecswap/releases/20261006-nu7-bb7fb0a`, which is now `/opt/zecswap/current`. That source uses
the librustzcash NU7 pre-releases and includes the gas alert code, inert without
`[gas_alerts]`. The relayer binary is unchanged from `e831bda`, and only the maker was
restarted. A build of `5f86999` ran from 04:18 to 04:27 UTC. It did not recognise already
imported joint accounts, which stopped the dashboard's flow observer, and `bb7fb0a` fixed
that. [sepolia-nu7-maker.json](../deployments/retired/sepolia-nu7-maker.json) records the checksums
and checks.

Its first start applied three schema migrations to `wallet.sqlite` and `flow-wallet.sqlite`
(71 to 74 applied). Builds from before NU7 cannot be assumed to open them. Copies taken
with the maker stopped, before the migrations, are in
`/var/backups/zecswap/20261006-nu7-5f86999` (root-only), with `previous-current`.
`/var/backups/zecswap/20261006-nu7-bb7fb0a` holds the migrated copies. Rolling back to a build
from before NU7 would bring the rejected transactions back. Roll back only with those copies,
and only if the testnet itself rolls NU7 back.

Before the cutover, the same wallet code sent a testnet self-payment. It was accepted as a v6
transaction with branch ID `0x77190ad9` and mined at block 4,469,704 (`c3af916a…`).
After the cutover, local and public maker health returned 204, relayer terms 200, and maker
info reported the same deployment. The flow observer completed its passes with no error,
with no warnings and no restarts. No reverse swap has run through the upgraded maker yet.
Android devices still run SDK builds from before NU7, so a device's own testnet
transactions, including the final sweep of a reverse swap, are rejected until that SDK is
updated.

## October 6 relayer funding fee

At 04:46 UTC the relayer binary was replaced by a build of source
`a53d6f29f5fae5f6ce0c4fce5d8a02ddd02a90bb` (#4), in
`/opt/zecswap/releases/20261006-fundfee-a53d6f2`, which is now `/opt/zecswap/current`. The release
carries the exact `bb7fb0a` maker binary, and the maker was not restarted. The relayer config gained
`fee = 250000` under `[reverse_funding]`, so sponsored reverse funding now has to transfer 0.25 test
tokens to the relayer in the same Relay Adapt action, and `GET /relayer/v1/terms` advertises
`reverseFunding.fee`. Clients that send the earlier three-call funding are rejected.
[sepolia-relayer-funding-fee.json](../deployments/retired/sepolia-relayer-funding-fee.json) records the
checksums and checks. The relayer gas account was topped up by 0.02 Sepolia ETH beforehand.

A first attempt at 04:44 UTC created the release directory with mode `0700`, so systemd could not
execute the relayer (status 203/EXEC). The deploy script restored the previous config and release and
restarted the relayer within seconds. Release directories must be `0755`.

Rollback files are in `/var/backups/zecswap/20261006-fundfee-a53d6f2` (root-only):
`relayer-config.toml` and `previous-current`. Restore the config as `root:zecswap-relayer`, `0640`,
point `/opt/zecswap/current` back at the previous release, and restart only the relayer.

## October 7 terms-hash contract

At 04:15 UTC `0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c` was deployed on Ethereum Sepolia from
`59b7d21` (#8), with Railgun's Sepolia proxy and a 600-second lock, paying in the existing test
token; [sepolia-terms-hash.json](../deployments/sepolia-terms-hash.json) records it. A smoke test
deposited, opened, lock-refunded and refunded a swap on it.

Sepolia now prices new state far higher than it did on October 2: the deployment used 30.2M gas
(4.2M then), and on it `open` takes 360k, `deposit` 249k and `refund` 308k. `forge script`
simulates with the repository's `cancun` rules, so its gas limit falls short: its first attempt
(`0xa62e35f8…`) ran out at 5.5M and deployed nothing. Deploy with the node's own estimate instead:
`cast send --gas-limit <estimate plus a margin> --create <bytecode ‖ constructor arguments>`.
The anvil fork the live suite runs on keeps the older prices, so check gas limits against Sepolia
itself, such as the relayer's funding sponsorship `max_gas_limit` (4M). The last sponsored funding
on the October 2 contract, after the repricing (`0xfcb85880…`, October 6 23:06 UTC), was
estimated at 3.13M and used 2.67M; the new contract's `openReverse` stores less, so the limit
stays.

At 04:42 UTC both hosted services switched to this contract, running binaries built from
`9459be3` in `/opt/zecswap/releases/20261007-terms-hash-9459be3`, now `/opt/zecswap/current`.
Every swap on the October 2 contract had settled, and its token balance was the maker's
inventory alone, so it held no user funds. With both services stopped, the maker withdrew its
74.495565 test USDC from it (`withdraw-inventory`) and deposited them into the new contract
(`add-inventory`). The maker started on a fresh store, keeping its `MAKER_ROOT_SECRET`, ETH key,
Zcash seed and wallet databases; its config gained `max_awaiting_deposit = 20`, and `[tokens]`
stays off until the app spends tokens. The relayer's `token` and `maker` moved to the top level,
where they bound every swap it sends. The October 2 manifest is in `deployments/retired/`.

Local and public maker info and relayer terms report the new contract, maker health returns 204,
a forward and a reverse quote came back through the gateway, and neither service restarted. No
swap has run on the new contract yet. The dashboard's `BRIDGE_TESTNET_CONTRACT` still selects
the October 2 contract until it is changed and the dashboard redeployed.

Rollback files are in `/var/backups/zecswap/20261007-terms-hash-9459be3` (root-only): both
configs, the retired `maker.sqlite`, `previous-current`, and copies of `wallet.sqlite` and
`flow-wallet.sqlite` taken with the maker stopped. Returning to the October 2 contract takes more
than the symlink: move the inventory back the same way, and restore both configs and the retired
store. Never restore the wallet copies over the live wallets.

## October 7 tokens

At 19:02 UTC the maker began taking Privacy Pass tokens ([tokens.md](tokens.md)). Both services
moved to `/opt/zecswap/releases/20261007-tokens-9b52dfc`, now `/opt/zecswap/current`: the maker and
the new `zecswap-issuer` are built from `9b52dfc` (its crates are `c8b6f8a`'s), and the relayer binary
is `9459be3`'s, unchanged and not restarted. The issuer runs as the `zecswap-issuer` service on
`127.0.0.1:8789` (`/etc/zecswap-issuer/config.toml`): name `zecswap-testnet-issuer`, three tokens a
day, attestation `insecure-test`. `deploy/nginx.conf` routes `/issuer/` to it, and the gateway worker
(version `1e82ab5d`) forwards `/issuer/`. The issuer's key is `/etc/zecswap-issuer/key.pem`, the
maker's return key `/etc/zecswap-maker/return-key.pem`;
[sepolia-tokens.json](../deployments/sepolia-tokens.json) lists both public halves, which the app
pins. The maker config gained `[tokens]`, spent tokens in
`/var/lib/zecswap-maker/spent-tokens.sqlite`, and the maker started on a fresh store: the build
refuses one from before tokens were handed back, and the previous store held no swap.

Public maker info shows `tokenReturnKey`, the issuer serves its key, and an accept without a token
answers `401` with a challenge for the day (20733). The owner's phone, on zapp-android `b9e77d165`
with the NU7 SDK branch, then ran a forward swap (`0xb709c357…`) on a token it fetched from the
issuer over Tor, which the maker handed back after the deposit and the phone collected with one
status read, and a reverse swap (`0x464e937b…`) on a token it held, handed back once the escrow was
funded. No returned token has been spent yet: that waits until a device's three issued tokens for
the day are gone.

Rollback files are in `/var/backups/zecswap/20261007-tokens-9b52dfc` (root-only): the maker config,
the previous `maker.sqlite`, the nginx site, `previous-current`, and the two public keys. Disable and
stop `zecswap-issuer`, restore the config as `root:zecswap-maker` `0640` and the store, point
`/opt/zecswap/current` back at the previous release, restart the maker, and restore and reload nginx.
Builds that pin the keys still swap with a maker that takes no tokens: they send a token only when
asked for one.

## October 8 attestation and gateway allowances

At 04:45 UTC the token issuer switched from `insecure-test` to `android-key`
([Token issuer attestation](#token-issuer-attestation)), running a build of `490eff4` in
`/opt/zecswap/releases/20261007-attestation-490eff4`, now `/opt/zecswap/current`. The release
carries the maker and relayer binaries of `20261007-tokens-9b52dfc` unchanged, and only the issuer
restarted. The issuer keeps its name, key and three tokens a day, so the app's pinned keys stand.
It counts tokens by each install's attested key, and takes the testnet packages signed with the
development debug key: `xyz.justzappit.zapp.testnet` and its `.debug`, `.foss.debug` and
`.internal.debug` builds, the key held at least in the phone's trusted environment. Google's two
attestation roots are in `/etc/zecswap-issuer/roots/`, and `zecswap-issuer-status.timer` refreshes
Google's status list into `/var/lib/zecswap-issuer/attestation-status.json` every six hours.

Two issuer fixes came with it. A certificate's `TRUE` written as 1, as a OnePlus StrongBox writes
it, no longer makes the certificate undecodable. The status list's serials written in decimal,
979 of its 1,759 entries, now count: the issuer had read only hex ones. A request captured from a
Galaxy A35 (locked, booted verified, key in its TEE) passes against Google's real roots, and none
of its certificates is on the status list.

The same deployment put the gateway's per-kind allowances in place
([Operating the deployment](#operating-the-deployment)), and the gateway Worker, now version
`14793e0d`, forwards only `Authorization`, `Content-Type` and `Content-Length`.
[sepolia-attestation.json](../deployments/sepolia-attestation.json) records the checksums and checks.

Only the app's testnet builds on a locked phone get tokens now. Emulators, phones with unlocked
bootloaders or self-signed boot (GrapheneOS, CalyxOS), `zecswap-cli`, and app builds that send
the earlier request get none, so they cannot accept a swap on the hosted maker. A release-signed
build needs its certificate's digest added to `signing_digests`.

On the Droplet the issuer answered its key and a challenge. Through nginx, maker health returned
204, maker info and relayer terms 200, and fifteen quick reads of one swap's status gave eleven
answers and four `503`s with `Retry-After`. Publicly, the issuer serves the pinned key and
refuses a request with a challenge it never gave out (`403`, "an unknown challenge") and one in
the earlier format (`400`), and an accept without a token still answers `401` with the day's
challenge. On the A35, an instrumented test's new key then got its three tokens through the
gateway, and its next fetch got `429`, the day's tokens spent. No swap has run on an attested token
yet.

Rollback files are in `/var/backups/zecswap/20261007-attestation-490eff4` (root-only): the issuer
config, the nginx site and its path, and `previous-current`. Disable `zecswap-issuer-status.timer`,
restore the config with `cp -a`, point `/opt/zecswap/current` back at the previous release,
restart the issuer, and restore and reload nginx. Roll the Worker back with
`npx wrangler rollback 1e82ab5d-7797-43ef-ba01-e0500a975b27`. The earlier issuer reads only the
earlier request, so builds that send the new one get no tokens from it.

## October 8 monitoring

At 14:55 UTC the maker, relayer and issuer restarted on a build of `991e16f` in
`/opt/zecswap/releases/20261008-monitoring-991e16f`, now `/opt/zecswap/current`. The maker's
`/v1/monitor` moved to schema 2 and reads nothing from the chain: `zapp-dashboard` now reads swap
states and balances itself, through Multicall3 on an RPC key of its own, so the dashboard never
spends the RPC the watchtower needs. The relayer and the issuer each serve a `/v1/monitor` of
their own, behind `RELAYER_MONITOR_TOKEN` (appended to `/etc/zecswap-relayer/secrets.env`) and
`ISSUER_MONITOR_TOKEN` (in the new root-only `/etc/zecswap-issuer/secrets.env`, which one added
line in the issuer's unit loads). The dashboard holds the same tokens in Vercel. The gateway gives
the three monitors one allowance of their own, and Alloy now ships the issuer's journal and its
status list refreshes as `service="zecswap-issuer"`. The maker's store gained two tables at start,
with no fresh store: swaps accepted before this release show no accept time.

The deploy script checked each staged file's SHA-256, refused to change anything unless the
Droplet was as the last deployment left it, and named any check that failed (a first run whose
checks failed silently changed nothing). After the restart the three monitors answered `401`
without their tokens and, with them, the maker's schema 2 (five swaps, none waiting on a deposit,
tokens on, ETH priced), the relayer's fees and the issuer's day (two devices served, six tokens,
both devices at their limit, a status list of 1,759 entries 2.8 hours old). Through nginx, maker
health returned 204, maker info, relayer terms and the issuer's key 200, the maker's monitor 200
with its token, and fifteen quick unauthenticated monitor reads gave eleven `401`s and four of the
gateway's `503`s. The redaction test passed its 33 fixtures against the new Alloy configuration
before Alloy restarted. The dashboard at `zapp-dashboard-seven.vercel.app`, deployed from
`zapp-dashboard` `4165055`, shows all three monitors healthy and the issuer's and maker's keys as
the app pins them. It raised one alert: the maker has no `[gas_alerts]`, so Telegram pages no one
when the maker's or the relayer's ETH runs low. All nine live scenarios passed on a fork of Ethereum
Sepolia with the live Zcash testnet, in 26.7 minutes, on the same commit.
[sepolia-monitoring.json](../deployments/sepolia-monitoring.json) records the checksums and checks.

Rollback files are in `/var/backups/zecswap/20261008-monitoring-991e16f` (root-only): the nginx
site and its path, `previous-current`, the issuer's unit, the relayer's `secrets.env`, and Alloy's
configuration, redaction patterns and scripts. Point `/opt/zecswap/current` back at the previous
release, restore the unit and the relayer's secrets with `cp -a`, remove
`/etc/zecswap-issuer/secrets.env`, `systemctl daemon-reload`, and restart the three services;
restore and reload nginx; restore Alloy's files and restart it. The earlier maker ignores the two
new tables, and the dashboard reads both monitor schemas.

## Deployment sequence

1. Deploy the updated contract on the chosen EVM testnet with the correct `RAILGUN` proxy (on
   Sepolia with `cast send --create` and the node's gas estimate; see October 7). Record the
   chain ID, contract, token, deployment transaction and block. Existing deployed contracts
   cannot gain reverse methods.
2. Configure maker and independent relayer for that exact deployment. Use a separate data
   directory for a new maker deployment; keep existing services and their pending swaps
   running until settled. A contract that stores only the terms' hash (October 6) needs a
   maker store created for it: an older `maker.sqlite` lacks each swap's token and `t0`, and
   the maker refuses to start on one. So does a store from before tokens were handed back
   (October 7), which lacks each swap's request for its token: let its swaps settle under the
   build that made it, then start the new build on a fresh store. A fresh store numbers maker shares from the clock, above
   any an earlier store handed out, so it may keep the same `MAKER_ROOT_SECRET`; the maker
   refuses to start if its secret or key is not the one its live swaps opened under. Enable
   `[reverse]` using the example maker config and supply
   `MAKER_ZCASH_SEED` alongside the existing maker secrets through the process environment.
   The relayer serves one token and one maker: set its `token` and `maker`.
3. Run `zecswap-maker --config <maker-config> zec-inventory` to obtain the seed-derived ZEC
   inventory address and balance. Fund the test inventory and the services' EVM gas accounts;
   to move the inventory from a retired contract, run `withdraw-inventory <amount>` under the old
   config and `add-inventory <amount>` under the new one. Write the token issuer's key with
   `zecswap-issuer keygen <key>`, and the maker's own return key, under which it hands tokens
   back, with a second `zecswap-issuer keygen <return-key>`: its public half goes into the app
   build, which pins it. Once the app spends tokens, give the maker a `[tokens]` table with the
   issuer's public half and the return key's file ([tokens.md](tokens.md)); a `[tokens]`
   without `return_key` no longer loads. Give the issuer an `[attestation.android-key]` table
   (see [Token issuer attestation](#token-issuer-attestation)); an issuer on `insecure-test`
   starts only with `allow_insecure = true`. Run
   `zecswap-issuer serve --config <issuer-config>`, `zecswap-maker --config <maker-config> serve`
   and `zecswap-relayer --config <relayer-config>` under host process supervision.
4. Configure a [Workers VPC Service](https://developers.cloudflare.com/workers-vpc/get-started/)
   for a remotely managed tunnel to the host. `deploy/nginx.conf` routes `/maker/`,
   `/relayer/` and `/issuer/` to loopback listeners. `deploy/worker/` supplies the stable `workers.dev`
   hostname; a custom domain is optional. Keep tunnel credentials and service secrets
   outside version control.
5. Validate the public endpoints below before updating the app's pinned deployment.

## Operating the deployment

Grafana Cloud telemetry setup, resource limits, credential handling and rollback
are documented in [Bridge observability](observability.md).

`scripts/build-server.sh` cross-compiles the Linux release binaries from macOS using the
pinned Rust toolchain, cargo-zigbuild and Zig. It needs `uv` and `rustup`. Upload the binaries
from `target/x86_64-unknown-linux-gnu/release/` to a new `/opt/zecswap/releases/<release>/`
directory, then switch `/opt/zecswap/current` and restart the services. Retain the previous
release for rollback; do not replace or roll back wallet databases during a binary rollback.

The systemd units are in `deploy/systemd/`. The deployed services are `zecswap-maker`,
`zecswap-relayer`, `zecswap-issuer` and `zecswap-tunnel`, all enabled at boot, with
`zecswap-issuer-status.timer`. The maker uses one proving
thread and two async workers so proving does not occupy the only API worker. Its state lives
in `/var/lib/zecswap-maker`; configs and root-readable environment
files live in `/etc/zecswap-maker`, `/etc/zecswap-relayer` and `/etc/zecswap-issuer`. The tunnel uses a systemd
credential loaded from `/etc/zecswap-tunnel/token`. API listeners are loopback-only; the
firewall permits inbound SSH. The host has 1 GB RAM, a 25 GB disk and a 1 GB swap file.

The maker config must be readable by its service account: root ownership, group
`zecswap-maker`, mode `0640`. Preserve that group and mode when replacing the file
atomically. Environment files can remain root-owned `0600`, since systemd reads them
before switching to the service account.

From `deploy/worker/`, run `npm ci`, `npm run types`, `npm run check`, `npm test`, then
`npx wrangler deploy --dry-run` before `npm run deploy`. The VPC binding is pinned in
`wrangler.jsonc`. The gateway streams request bodies unchanged, disables caching and does
not retry financial requests. It forwards only the `Authorization`, `Content-Type` and
`Content-Length` headers, so it passes on nothing else a client sends, nor the client address
Cloudflare attaches to the request.

Every request reaches nginx from `cloudflared` on loopback, so nginx can't tell callers apart,
and limits are never per IP. `deploy/nginx.conf` gives each kind of request its own allowance
instead, so a flood of one kind never refuses another:

| Requests | Allowance |
| --- | --- |
| Settling funded swaps (locks, claims, payouts, refunds, rescues): every relayer `POST` route except `reverse/fund` | 5 a second; up to 50 more wait their turn |
| Starting swaps: quotes, accepts and `reverse/fund` | 2 a second, bursts of 20 |
| The issuer's challenges and tokens | 2 a second, bursts of 20 |
| Reverse swap status, which reads the chain on every request | 2 a second, bursts of 20 |
| Everything else | 10 a second, bursts of 50 |

Each swap's status also has its own allowance, of one a second with bursts of ten, so an app
polling its swap too fast slows only that swap. Past an allowance nginx answers `503` with
`Retry-After` and the gateway's `unavailable` error, which clients take as a passing failure;
the services' own `503`s pass through unchanged. It never answers `429`, which from the issuer means a device's
tokens for the day are spent.

Keep the maker seed, root secret, and both databases together in protected backups. A disk
on the VPS is persistent storage, not an off-host backup. Restoring an old snapshot also
requires reconciling newer on-chain swaps before admitting new quotes.

A forward accept spends maker gas before the user's ZEC deposit, and a reverse accept reserves
the maker's ZEC until its funding deadline. The maker's `max_awaiting_deposit` caps how many
swaps wait on their users at once, in both directions. With `[tokens]`, every accept spends a
Privacy Pass token from `zecswap-issuer`, handed back once the user pays in, so a device walks
away from at most `tokens_per_day` swaps a day ([tokens.md](tokens.md)); that limit binds because
the issuer checks Android key attestation, as the hosted testnet issuer has since October 8
([Token issuer attestation](#token-issuer-attestation)). The relayer serves only its configured token and maker, on every
route, and every swap it serves paid for its accept; the funding route keeps its transaction
validation and gas caps, and there is no aggregate sponsorship budget. nginx's allowances are
per kind of request, not per caller: a flood of claims for swaps that don't exist still fills
the claims allowance, since the relayer reads the chain to tell them from real ones. Rely on
the cap and the tokens, not on them. Keep the existing forward ordering:
depositing ZEC before escrow exists would remove its contract-backed recovery path.

### Token issuer attestation

With `[attestation.android-key]`, the issuer gives tokens only to an install of the app whose
key the phone's secure hardware made and certifies up to Google's root, on a phone with a locked
bootloader that booted verified, and it counts each day's tokens by that key. Scripts, emulators,
phones with unlocked bootloaders and other apps get none, and an install gets no more by asking
more often. It does not stop someone with a genuine phone from reinstalling the app, or clearing
its data, for a new key and a fresh allowance; closing that needs a check tied to the phone, such
as Play Integrity's device recall, or a bond. Nor does it hold against someone who breaks into a
locked phone's running system.

The issuer needs, readable by its service account: Google's attestation roots as PEM files
(`roots`), saved from `https://android.googleapis.com/attestation/root`; Google's status list
(`status_list`), saved from `https://android.googleapis.com/attestation/status` and refreshed at
least daily by a timer that writes a new file and renames it into place (the issuer rereads it
when it changes, and refuses to start if it can't read it); the app's package (testnet builds are
`xyz.justzappit.zapp.testnet`) and the SHA-256 of its signing certificate. The issuer's `name`
is in every install's key: changing it makes every install start over with a new key.
`deploy/systemd/zecswap-issuer-status.timer` refreshes the list every six hours: its service
puts a new list in place only once it reads.

Run the issuer apart from the maker, ideally by another party: it sees each install's key on every
fetch, and the maker must never see an attestation. It logs no chain, key, challenge or device id,
and keeps only today's counts.

The hosted testnet issuer has run `android-key` since October 8
([October 8](#october-8-attestation-and-gateway-allowances)). `insecure-test` believes any caller,
and the issuer starts in it only with `allow_insecure = true`.

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
60-second cache and a single in-flight request. The same request carries ETH (ID 1027) for
the dashboard's gas values alone: missing or stale, it is left out and never touches quotes.
There is no cron or background polling.
ZEC/USDC is ZEC/USD divided by USDC/USD, converted to six-decimal token units with decimal
arithmetic; the existing spread and zatoshi rounding apply in both swap directions.
Both asset timestamps must be within the configured maximum age. A provider failure can
reuse the last valid price only within that limit; without one, new quotes return 503
with `unavailable`. Provider retries are limited to once per ten seconds after failures.
The old fixed price is ignored whenever market pricing is enabled. Issued quotes retain
their exact stored amounts until expiry; settlement and watchtower recovery continue
independently of the price feed. Omitting `[pricing.market]` retains fixed pricing for
local tests. Production and mainnet should explicitly enable market pricing.

`GET /maker/v1/monitor` is a read-only operations export for `zapp-dashboard`, and
`GET /maker/v1/monitor/swaps/{id}` the same record for any one swap, old or new. Both are
disabled unless `MAKER_MONITOR_TOKEN` is set (at least 32 characters), and require
`Authorization: Bearer <token>`. Use the same value as `BRIDGE_TESTNET_MONITOR_TOKEN`
in the dashboard's server environment. No seed, spending share, viewing key, user
authorization, or wallet account identifier is returned.

The export (`schemaVersion` 2) holds only what the maker knows: shielded ZEC total/spendable/
reserved/available inventory and the ZEC sitting in unsettled swaps' deposit accounts, quote
and swap counts and how many swaps wait on a deposit (against `max_awaiting_deposit`),
watchtower readiness, sync recency, errors since restart, pricing/timing policy, the gas
alert accounts, the token gate's day (see [tokens.md](tokens.md)), and up to 50 swaps with
active records first. The `pricing` object reports the same prices used for quotes, provider
timestamps, cache/freshness policy and sanitized refresh errors. USD inventory values and
displayed maker buy/sell rates use these readings, rather than another independent price feed.
Each swap carries its quote ID, when it was accepted, opened and settled, whether it is
archived or cancelling, whether its token went back, deposit/sweep transaction IDs, wallet
funds, deadlines, and every recorded Ethereum event with its block time. Settled counts
include expired and refunded swaps; they are not successful-trade counts. Per-swap errors are
from the last completed watchtower pass; the counter resets when the maker restarts. Detailed
errors remain in the maker logs.

The endpoint reads nothing from the chain: contract states, USDC and ETH balances are the
dashboard's to read, on its own RPC, so monitoring never spends the RPC the watchtower needs.
It never syncs the wallet, generates proofs, sends transactions, or writes inventory. A busy
wallet uses the last captured inventory reading and sets `walletBusy`; individual wallet
readings remain unknown while busy.

The relayer's `GET /relayer/v1/monitor` (`RELAYER_MONITOR_TOKEN`) counts, for each operation
since it started, the transactions sent, the requests refused and those that failed (by kind:
`reverted`, `unconfirmed` or `internal`, never a message), and the gas and wei its sent
transactions burned, from which the dashboard works out how long its ETH lasts. The issuer's
`GET /issuer/v1/monitor` (`ISSUER_MONITOR_TOKEN`, in `/etc/zecswap-issuer/secrets.env`)
shows the day's token totals, its refusals by reason and its status list's age. Both are
closed while their token is unset and take tokens of at least 32 characters. All three
monitors share a gateway allowance of their own (one a second, a burst of ten), apart from the
apps'.

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
evidence is in [retired deployment smoke evidence](../deployments/retired/sepolia-reverse-bd9a37-smoke.json).
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
