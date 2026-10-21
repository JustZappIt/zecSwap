# Public testnet services

The maker, relayer and token issuer run on the DigitalOcean Droplet at `<droplet ip>`. A Cloudflare Worker
at `https://<testnet gateway>.workers.dev` forwards requests through a private Workers
VPC Service and a named Cloudflare Tunnel. The phone needs neither USB forwarding nor a
running development Mac. The databases remain on the Droplet's persistent disk.

| API | Base URL |
| --- | --- |
| Maker | `https://<testnet gateway>.workers.dev/maker` |
| Relayer | `https://<testnet gateway>.workers.dev/relayer` |
| Token issuer | `https://<testnet gateway>.workers.dev/issuer` |

Both services are on the same host for this testnet deployment, under separate Unix users
and signing keys. This does not provide independent operators: the production relayer must
remain independent of the maker, as the protocol requires.

Both hosted services use the Sepolia windows contract
[`0x459b0a69e0f736666b66a3ffe0fc17c11f05980d`](https://sepolia.etherscan.io/address/0x459b0a69e0f736666b66a3ffe0fc17c11f05980d),
on which at every moment at most one side may reveal its share
([October 20](#october-20-windows-contract)); [sepolia-windows.json](../deployments/sepolia-windows.json)
records it. Before it, from October 7, they used
[`0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c`](https://sepolia.etherscan.io/address/0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c),
which stores only a hash of each swap's terms ([October 7](#october-7-terms-hash-contract)).
[sepolia-terms-hash.json](../deployments/retired/sepolia-terms-hash.json) records the transaction,
block, test token, Railgun proxy, lock duration and the services' release. Since 19:02 UTC that
day the maker takes Privacy Pass tokens ([October 7 tokens](#october-7-tokens)): Android builds
that pin this contract and both token keys can swap; earlier builds cannot. Since 04:45 UTC on
October 8 the issuer gives tokens only to installs whose key a locked phone's secure hardware
attests, and the gateway gives each kind of request its own allowance
([October 8](#october-8-attestation-and-gateway-allowances)). Since 03:32 UTC on October 12 each
swap's relayer requests can carry its ticket for an allowance of their own
([October 12](#october-12-tickets-and-network-names)), and since 04:48 UTC the relayer sends a
Telegram message for each private send and unshield, and the maker one when its account or the
relayer's runs low on ETH ([October 12 alerts](#october-12-relayer-and-gas-alerts)). Since 08:03
UTC their sends pay the gas price quotes and fees charge
([October 12 gas](#october-12-sends-at-the-quoted-gas-price)), and since 08:31 UTC each watches
its own account's ETH, read when one of its sends ends and on a timer only while it is low
([October 12 gas alerts](#october-12-gas-alerts-on-sends)). Since 09:09 UTC a claim or payout
posted again never sends a second transaction or answers `500`, and the gateway lets ten accepts in
at a time ([October 12 concurrent swaps](#october-12-concurrent-swaps)). Since 18:31 UTC a pass
reads all its swaps in one call and never again one a finalized block holds settled, and both
services log and export what they ask their RPC ([October 12 RPC budget](#october-12-rpc-budget)).
Since about 22:59 UTC on October 17 the maker's sweeps carry no outgoing copy a swap's user could
read, as on mainnet since 23:00 ([October 17 sweeps](#october-17-sweeps-without-an-outgoing-copy)).
Since 23:42 UTC on October 18 both swap a new test token, which the relayer also takes at wallets'
deposit addresses and shields into their Railgun balances
([October 18 deposits](#october-18-deposits-and-a-new-test-token)). Since 02:27 UTC on October 19 a
swap can be up to 200 tUSD, up from 20 ([October 19](#october-19-a-larger-swap-cap)). Since 00:01
UTC on October 21 both run on the windows contract, and app builds that speak locks can no longer
swap ([October 20](#october-20-windows-contract)).

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
swap remain unverified; follow the [funding API handoff](reverse-flow.md#sponsored-initial-funding).

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
token; [sepolia-terms-hash.json](../deployments/retired/sepolia-terms-hash.json) records it. A smoke test
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

Since 2026-10-14 04:42 UTC the testnet maker asks for no tokens, as the owner asked, so any phone,
emulator or the CLI can swap there: its `[tokens]` table is commented out, the config with it is
`/etc/zecswap-maker/config.toml.bak-20261013-tokens`, and public info no longer shows
`tokenReturnKey`. The app posts an accept without a token first, so it needed no change. The issuer
still runs; uncomment the table and restart the maker to ask for tokens again.

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
`npx wrangler rollback <worker version>`. The earlier issuer reads only the
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

## October 8 Railgun sends

At 19:10 UTC the relayer restarted on a build of `970cdf9` in
`/opt/zecswap/releases/20261008-railgun-sends-970cdf9`, now `/opt/zecswap/current`. It sends
wallets' own private Railgun sends and withdrawals as their broadcaster
(`POST /relayer/v1/railgun/transact`, [railgun-sends.md](railgun-sends.md)) for a fee note of
0.25 test tokens to its own Railgun wallet, `0zk1qy25r4x4…xpwljug`, whose seed was appended to
`/etc/zecswap-relayer/secrets.env` as `RELAYER_RAILGUN_SEED`. Its config gained `[railgun_sends]`
(fee 250000, gas capped at 3M and 20 gwei, journal `/var/lib/zecswap-relayer/railgun-sends.sqlite`),
and the gateway gave the route an allowance of its own and the 132k body limit. The release
carries the `991e16f` maker and issuer binaries byte for byte; neither restarted.

The deploy script checked the staged files and that the Droplet was as the monitoring release left
it, then backed up the relayer's config and secrets, the release link and the nginx site. After the
restart the relayer's terms advertised the Railgun wallet, Sepolia's proxy and the fee, its monitor
counted eleven operations and the sends fee, and it refused a send for another chain. Through nginx,
maker health returned 204, maker info, relayer terms and the issuer's key 200; a 60 KB send reached
the relayer and a 140 KB one got `413`. Through the public gateway the terms advertise
`railgunSends`, and a send for another chain or calldata that is no `transact` call are refused
with `400`. `railgun-send` passed live on Ethereum Sepolia and, with all nine other scenarios, on a
fork of it. On Sepolia a relayed send's outputs stay unscreened: Railgun's indexer there breaks its
chain of transaction verification hashes at index 4188, so wallets cannot prove their own sends
(mainnet's chain is whole). [sepolia-railgun-sends.json](../deployments/sepolia-railgun-sends.json)
records the checksums and checks.

Rollback files are in `/var/backups/zecswap/20261008-railgun-sends-970cdf9` (root-only):
`relayer-config.toml`, `relayer-secrets.env`, `nginx-site` and `previous-current`. Restore the
config as `root:zecswap-relayer`, `0640`, and the site, point `/opt/zecswap/current` back at the
previous release, reload nginx and restart only the relayer.

At 22:05 UTC the relayer restarted on a build of `5f82797` in
`/opt/zecswap/releases/20261008-railgun-spent-5f82797`, now `/opt/zecswap/current`: a `409
alreadySpent` from `/relayer/v1/railgun/transact` now names the relayer's own transactions that
spend the notes. Only the binary changed; the config, secrets and gateway are as `970cdf9` left
them, and the maker and issuer were not restarted. Afterwards the terms advertised the Railgun
sends, the monitor counted the sends fee, and a send for another chain was refused directly,
through nginx and through the public gateway.
[sepolia-railgun-spent.json](../deployments/sepolia-railgun-spent.json) records the checksums. To
roll back, point `/opt/zecswap/current` at the release in
`/var/backups/zecswap/20261008-railgun-spent-5f82797/previous-current` and restart the relayer.

Rollback files are in `/var/backups/zecswap/20261008-monitoring-991e16f` (root-only): the nginx
site and its path, `previous-current`, the issuer's unit, the relayer's `secrets.env`, and Alloy's
configuration, redaction patterns and scripts. Point `/opt/zecswap/current` back at the previous
release, restore the unit and the relayer's secrets with `cp -a`, remove
`/etc/zecswap-issuer/secrets.env`, `systemctl daemon-reload`, and restart the three services;
restore and reload nginx; restore Alloy's files and restart it. The earlier maker ignores the two
new tables, and the dashboard reads both monitor schemas.

## October 9 profitability

At 03:37 UTC the maker and relayer restarted on builds of `8c81c9a` in
`/opt/zecswap/releases/20261009-profitability-8c81c9a`, now `/opt/zecswap/current`; the release
carries the `991e16f` issuer byte for byte, and it did not restart. The maker's `[pricing.market]`
gained `providers = ["coinmarketcap", "alchemy"]`, and it records what each swap earned and cost
(`GET /maker/v1/monitor/economics`). The relayer records what each sponsored send cost
(`GET /relayer/v1/monitor/sends`) and prices sends by gas: `[railgun_sends]` gained the same
`providers` and `fee_margin_bps = 1000`, and its `fee`, now the floor, rose from 250000 to 500000.
Both services' `secrets.env` gained `ALCHEMY_API_KEY`, the key of the Alchemy URL the maker's RPC
already used, and the relayer's the maker's `ZCASH_CMC_KEY`. The gateway is unchanged.

The deploy script checked the staged files, that the Droplet was as `5f82797` left it and that
Alchemy's Prices API priced ZEC, USDC and ETH with that key, then backed up both configs and
secrets, `maker.sqlite` and the relayer's sends journal. After the restart the relayer's terms
quoted the 500000 floor and 2,736 test tokens an ETH of gas, margin included, for ten minutes,
directly and through nginx; its ledger had costed both sends it had sent; the maker priced from
CoinMarketCap, fresh, with neither provider failing, and its economics export listed eleven swaps
of the last 30 days; both exports answered through nginx, and a send for another chain was
refused. Through the public gateway the terms quote the floor and the rate, and maker health
returns 204. zapp-dashboard's `/bridge/profit`, deployed first (`d4a12d8`, production deployment
`aue6rmdyt`), charts both. The test token emits no `Transfer` events, and the export reads a
relayer's fee from the transfers to it, so here it counts the relayer's payout fees (0.10 each) as
nothing. [sepolia-profitability.json](../deployments/sepolia-profitability.json) records the
checksums and checks.

Rollback files are in `/var/backups/zecswap/20261009-profitability-8c81c9a` (root-only): both
configs and `secrets.env` files, `maker.sqlite`, `railgun-sends.sqlite` and `previous-current`.
Restore the configs and secrets with `cp -a`, point `/opt/zecswap/current` back at the previous
release and restart the maker and relayer; the earlier binaries ignore the new tables, so the
stores stay. Apps from before zapp-android `5f6a4529b` cap a send's fee at 0.25 and so see private
sends as unavailable while the floor is 0.50.

## October 9 swap fees

At 04:40 UTC the maker and relayer restarted on builds of `546faa8` in
`/opt/zecswap/releases/20261009-swap-fees-546faa8`, now `/opt/zecswap/current`; the issuer was not
restarted. Every swap now pays its way ([Each swap pays its own network
cost](#each-swap-pays-its-own-network-cost)): the maker's config gained `[pricing.costs]`
(450,000 gas forward, 400,000 reverse, 15,000 zatoshis, 20% margin), and the relayer's
`providers` and `fee_margin_bps` moved from `[railgun_sends]` to the top level beside
`fee_gas = 1800000`, with `[reverse_funding] fee_gas = 2300000`. With the old maker stopped, its
ledger forgot the 26 relayer transactions it had read as paying nothing, and the new one read
them again from the escrow's events: the eight payouts now carry the relayer's 0.10 fee, which
the test token's missing `Transfer` logs had hidden.

The deploy script checked the staged files, that the Droplet was as `8c81c9a` left it and both
configs' shape, backed up both configs and `maker.sqlite`, and after the restart saw the
relayer's terms quote its payout and funding fees for an hour (at their floors, 100000 and
250000, with Sepolia's gas that cheap) and its sends' rate, directly and through nginx; a test
quote for one unit charge a network cost of 221,484 (18,294 zatoshis in all); the maker's
monitor show the costs; and both exports answer through nginx. Through the public gateway the
terms quote the fees with an hour to run, and the dashboard reads the maker's costs and the
relayer's expiry. [sepolia-swap-fees.json](../deployments/sepolia-swap-fees.json) records the
checksums. Apps from before the change to the relayer's fee (the 0.10 `MAX_RELAYER_FEE` cap)
refuse to claim once gas lifts the payout fee over 0.10, about 0.02 gwei on Sepolia.

Rollback files are in `/var/backups/zecswap/20261009-swap-fees-546faa8` (root-only): both
configs, `maker.sqlite` and `previous-current`. Restore the configs with `cp -a`, point
`/opt/zecswap/current` back at the previous release and restart the maker and relayer: the
previous relayer refuses this config, and the previous maker ignores `quote_costs`.

## October 12 tickets and network names

At 03:32 UTC the maker, relayer and issuer restarted on builds of `d7afd11` in
`/opt/zecswap/releases/20261011-tickets-d7afd11`, now `/opt/zecswap/current`; the release before it
was `20261009-price-history-46d2db9`. The relayer checks that a wallet's calldata is its call's
canonical encoding before decoding any of it, takes withdrawals of its own token alone, and holds a
reverted send's notes for an hour; each kind of send leaves from its own account. Each accept
answers with a ticket: the maker's `secrets.env` gained `MAKER_TICKET_SECRET`, and the Worker the
same value as `TICKET_SECRET` (version `485414be`), with no other copy kept. The relayer's and the
issuer's configs gained `network = "testnet"`, and the relayer keeps Railgun's own Relay Adapt.
nginx gives each ticketed swap's relayer requests an allowance of their own;
`$zecswap_tickets_required` stays 0 until the app sends tickets.

The deploy script checked the staged files and that the Droplet was as `46d2db9` left it, backed up
the relayer's and issuer's configs, the maker's `secrets.env`, the nginx site, `maker.sqlite` and the
relayer's journal, ran `nginx -t`, and after the restart saw through nginx the maker's info
(testnet, Sepolia), the relayer's terms, the issuer's key, and claims reach the relayer with a swap
named and without. Through the public gateway, 15 claims at once with a checked ticket for one swap
saw 11 reach the relayer and 4 refused, and with a forged ticket all 15 reached it.
[sepolia-tickets.json](../deployments/sepolia-tickets.json) records the checksums.

Rollback files are in `/var/backups/zecswap/20261011-tickets-d7afd11` (root-only). Restore the
relayer's and issuer's configs, the maker's `secrets.env` and the nginx site with `cp -a`, point
`/opt/zecswap/current` back at the previous release, reload nginx and restart the three services:
the previous relayer and issuer refuse `network`. The Worker's previous version ignores
`TICKET_SECRET`.

Each kind of send left from its own account for the first time, and the relayer kept each account's
float at the gas price, while a send holds its gas at the fee cap it is filled with: on Sepolia
hundreds of times the gas price. Within a minute a user's refund lock found the locks account
holding 0.000008 ETH where it reserves 0.00003, and from 03:39 UTC the stopgap
`top_up_transactions = 3000` in the relayer's config kept every account sending. The relayer of
`e4d43fc`, in `/opt/zecswap/releases/20261011-lane-float-e4d43fc` from 03:53 UTC, keeps each
float at the fee cap instead, and the stopgap line is gone; only the relayer changed.
[sepolia-lane-float.json](../deployments/sepolia-lane-float.json) records it, and
`/var/backups/zecswap/20261011-lane-float-e4d43fc` the config with the stopgap.

## October 12 relayer and gas alerts

At 04:48 UTC the maker and relayer restarted on builds of `0d4cdb3` in
`/opt/zecswap/releases/20261012-relayer-alerts-0d4cdb3`, now `/opt/zecswap/current`; the issuer is
`e4d43fc`'s, unchanged. The relayer sends a Telegram message for each Railgun private send and
unshield it broadcasts, once the send mines. A drop-in,
`/etc/systemd/system/zecswap-relayer.service.d/telegram.conf`, gives it the maker's
`/etc/zecswap-maker/telegram.env`: the same bot and chat. The maker's config gained
`[gas_alerts]`: every five minutes it reads the ETH of its own account
(`0x2bac02B5032e9092493814c705F156B49E288922`) and the relayer's
(`0xd9633572041886Fa7584a2E12F36C8C7f1126412`, which keeps its sending accounts topped up), with a
message under 0.005 ETH and another once back at 0.01. They held 0.0118 and 0.0109 ETH, so the
first reading sent nothing.

The deploy script checked the staged files and that the Droplet was as `e4d43fc` left it, backed up
the maker's config, `maker.sqlite` and the relayer's journal, and after the restart saw through
nginx the maker's info and the relayer's terms, the relayer's `alerts=true`, and both accounts
in the maker's monitor. A labeled test message put on the running relayer's own queue reached
Telegram on its first attempt. [sepolia-relayer-alerts.json](../deployments/sepolia-relayer-alerts.json)
records the checksums.

Rollback files are in `/var/backups/zecswap/20261012-relayer-alerts-0d4cdb3` (root-only). Restore
the maker's config with `cp -a`, remove the relayer's drop-in and reload systemd, point
`/opt/zecswap/current` back at the previous release and restart the maker and relayer. The
previous relayer ignores the queue's tables in its journal.

## October 12 sends at the quoted gas price

At 08:03 UTC the maker and relayer restarted on builds of `86651c2` in
`/opt/zecswap/releases/20261012-node-tip-86651c2`, now `/opt/zecswap/current`; the issuer is
`0d4cdb3`'s, byte for byte, and did not restart. Every contract call they send tips what the node
suggests, the tip in the gas price that quotes and the relayer's fees charge, under a cap of twice
the next block's base fee ([Each swap pays its own network cost](#each-swap-pays-its-own-network-cost)).
alloy's own estimate had tipped the median of recent blocks' tips: on Sepolia 0.5 to 1 gwei half
the time, against the 0.001 gwei quotes charged. The stress run's five 1 tUSD swaps at 05:45 UTC
lost $17.82 between the maker and the relayer, and the relayer's own account, which pays every
payout, fell to 0.0013 ETH before the owner topped it up to 0.104. The maker's ledger also credits
a sponsored funding's fee to the relayer when its funding account sends it. No config changed.

The deploy checked that the Droplet ran `20261012-relayer-alerts-0d4cdb3` and the staged
checksums, backed up `maker.sqlite` and the relayer's journal, and after the restart saw the
relayer's terms and the maker's info answer within three seconds, maker health return 204 after
36, both services answer through the public gateway, and no warning or error in either journal.
No transaction had been sent since the restart when this was recorded; the next swap's sends are
the first at the node's tip. [sepolia-node-tip.json](../deployments/sepolia-node-tip.json) records
the checksums.

Rollback files are in `/var/backups/zecswap/20261012-node-tip-86651c2` (root-only):
`maker.sqlite`, `railgun-sends.sqlite` and `previous-current`. Point `/opt/zecswap/current` back at
the previous release and restart the maker and relayer; the stores didn't change, so they stay.

## October 12 gas alerts on sends

At 08:31 UTC the maker and relayer restarted on builds of `c1b865c` in
`/opt/zecswap/releases/20261012-gas-on-sends-c1b865c`, now `/opt/zecswap/current`; the issuer is
`0d4cdb3`'s, byte for byte, and did not restart. Gas alerts no longer read balances every five
minutes. Each service watches the account it sends from, read when it starts and each time one of
that account's sends ends, mined or failed, and every 300 s only while it is under 0.005 ETH, until
a top-up brings it back to 0.01. The maker's `[gas_alerts]` kept its thresholds and lost its
account list, and the relayer's config gained the same section, whose messages queue in its
Railgun sends journal. zapp-dashboard `43fb091`, deployed to Vercel production first, reads the
relayer's gas alerts from the relayer's own monitor.

The deploy checked that the Droplet ran `20261012-node-tip-86651c2` and the staged checksums,
backed up both configs, `maker.sqlite` and the relayer's journal, and after the restart saw the
maker's info and the relayer's terms through nginx, and each monitor name its own account alone
for gas alerts. The maker held 0.0579 ETH and the relayer 0.0831, so the first readings queued
nothing; none failed, neither journal logged an error, and the dashboard shows no alert.
[sepolia-gas-on-sends.json](../deployments/sepolia-gas-on-sends.json) records the checksums.

Rollback files are in `/var/backups/zecswap/20261012-gas-on-sends-c1b865c` (root-only):
`maker-config.toml`, `relayer-config.toml`, `maker.sqlite`, `railgun-sends.sqlite` and
`previous-current`. Restore both configs, since the older binaries refuse the new `[gas_alerts]`,
point `/opt/zecswap/current` back at the previous release, and restart the maker and relayer.

## October 12 concurrent swaps

At 09:09 UTC the relayer restarted on a build of `36a3c1a` in
`/opt/zecswap/releases/20261012-concurrent-swaps-36a3c1a`, now `/opt/zecswap/current`, and nginx
reloaded the gateway config of the same commit; the maker is `c1b865c`'s and the issuer `0d4cdb3`'s,
byte for byte, and neither restarted. Fifteen swaps at once had shown the way. Clients whose claims
timed out, while every payout queued behind one stuck in the pool, posted their payouts again, and
the relayer sent second ones, which reverted or failed their simulation, and answered `500`. Now a
swap's requests take turns and run to their end whether or not their clients still wait, and a
swap's claim, payout, ready, refund and refund payout each go out once: a repeat while the first
may still land answers `503`, and once it has, `200` with no transactions. A relayer that restarts
sends none of them from an account whose earlier sends aren't mined. `zecswap-cli` of this commit
follows a relayer request that timed out on the chain and posts it again, and waits out the
gateway's `503`. The gateway gives quotes and accepts zones of their own and lets ten accepts in at
a time ([Operating the deployment](#operating-the-deployment)).

The deploy checked that the Droplet ran `20261012-gas-on-sends-c1b865c`, the staged checksums and
the installed site's, and that nginx took the new site on its own, backed up the site and the
relayer's journal, and after the restart saw the maker's info and the relayer's terms through
nginx, and a quote and an accept each reach the maker through its own zone. No error since the
restart. [sepolia-concurrent-swaps.json](../deployments/sepolia-concurrent-swaps.json) records the
checksums.

Rollback files are in `/var/backups/zecswap/20261012-concurrent-swaps-36a3c1a` (root-only):
`nginx-site`, `railgun-sends.sqlite` and `previous-current`. Restore the site with `cp -a` and
reload nginx, point `/opt/zecswap/current` back at the previous release and restart the relayer;
its journal didn't change.

## October 12 RPC budget

At 13:48 UTC the Alchemy account behind the services' RPC (key `3956e467`, shared with
`.env.testnet` and the Android dev builds) hit its monthly cap of 30M compute units and answered
every call `429`, so quotes paused. Nearly all of it was the maker. Each pass read every swap settled
in the last day, three calls a swap: after the day's stress runs, 42 swaps, 8.7 requests a second,
about 16M compute units a day. The transaction observer read the newest twelve blocks' logs again
every 15 seconds, about 1M a day, idle or not. At 17:53 UTC the maker and relayer moved to another
Alchemy key (`f6773aad`); their earlier configs and secrets are beside them as
`*.bak-20261012-alchemy-key`.

At 18:31 UTC the maker and relayer restarted on builds of `c42ac51` in
`/opt/zecswap/releases/20261012-rpc-budget-c42ac51`, now `/opt/zecswap/current`; the issuer is
`0d4cdb3`'s, byte for byte, and did not restart. A pass reads all its swaps in one call, and never
again one whose claim or refund a finalized block holds; with nothing to read, it makes one
`eth_chainId`, which Alchemy bills nothing ([RPC use](#rpc-use)). Each service logs its RPC at start,
by host and key fingerprint, and its requests every hour, and both monitors export `rpc`. No config
changed.

The deploy checked the staged checksums and that the Droplet ran
`20261012-concurrent-swaps-36a3c1a`, backed up both configs, `maker.sqlite` and the relayer's
journal with both services stopped, and after the restart saw both log the key's fingerprint, maker
health return 204, the maker's info and the relayer's terms through the public gateway, and the
dashboard's checks all healthy, with no warning or error in either journal. The maker's first pass
found the 42 settled swaps finalized and has read none of them since. In its first two and a half
minutes it sent 32 requests (620 compute units, most at startup), against 8.7 requests and about 180
compute units a second before; the relayer sent 30 in six minutes.
[sepolia-rpc-budget.json](../deployments/sepolia-rpc-budget.json) records the checksums. A stress run
then restarted the maker on this release with `[tokens]` commented out for its CLI clients;
`/etc/zecswap-maker/config.toml.pre-stress` holds the config with them.

Rollback files are in `/var/backups/zecswap/20261012-rpc-budget-c42ac51` (root-only):
`maker-config.toml`, `relayer-config.toml`, `maker.sqlite`, `railgun-sends.sqlite` and
`previous-current`. Point `/opt/zecswap/current` back at the previous release and restart the maker
and relayer; the stores didn't change, so they stay.

## October 14 testnet inventory and monitor

At 17:45 UTC the maker, stopped for a minute, minted 1,000 test USDC and deposited them
(`add-inventory 1000000000 --mint`: mint `0xa49773c5…`, approve `0x5e83a9ab…`, deposit
`0x97ae7f45…`, Sepolia blocks 11914834–11914836), taking its inventory from 28.642822 to
1,028.642822. ZEC → USDC swaps had had room for one at the 20 USDC cap.

The dashboard's maker telemetry check then failed now and then ("Unavailable, timed out, or
invalid response"). With its Zcash wallet idle, the maker took about 9 seconds to answer its
monitor, against the dashboard's 12: every read took a wallet summary, which covers every account,
once for each of the 50 swaps it lists, again for the inventory and for each unsettled swap. With
the wallet busy it skipped them all and answered in 0.05 s, so the check came and went. The
watchtower likewise took a summary per watched swap after each sync, holding the wallet meanwhile.

At 20:17 UTC the testnet maker restarted on a build of `1765bb6` in
`/opt/zecswap/releases/20261014-monitor-funds-1765bb6`, now `/opt/zecswap/current`, which reads
every account's funds from one summary. The relayer and issuer are the previous release's, byte for
byte, and did not restart; no swap was in progress and no config changed. Six monitor reads ten
seconds apart then took 0.15–0.51 s, the dashboard's check read healthy at 561 ms, and the maker
logged no warning or error. [sepolia-monitor-funds.json](../deployments/sepolia-monitor-funds.json)
records the checksums. The mainnet maker still reads funds the old way.

Rollback files are in `/var/backups/zecswap/20261014-monitor-funds-1765bb6` (root-only):
`maker-config.toml` and `previous-current`. Point `/opt/zecswap/current` back at the previous
release and restart the maker; the store didn't change.

## October 17 sweeps without an outgoing copy

Both sides of a swap hold its joint account's viewing key, and every sweep from a joint account
sealed its output's outgoing copy under that account's outgoing key. So each swap's user could
read where the maker swept its ZEC, a claimed deposit or a refunded reverse escrow, and how much:
the maker's `sweep_to`, on mainnet its own ZEC inventory, or any wallet named there later. Since
`1faaf63` (`fix/sweep-outgoing-copy`) a sweep carries no outgoing copy. The live suite's `happy`,
`underpaid` and `never-claimed` passed with it on Base Sepolia and the Zcash testnet on October 16
and 17; its makers' sweeps `af07fe73…` and `936e1ce2…` and the CLI's refund sweeps `1daac90a…`
and `36d28930…` were mined.

At about 22:59 UTC the testnet maker restarted on a build of `05cab29`, the `1765bb6` it ran with
`1faaf63` applied, in `/opt/zecswap/releases/20261017-sweep-outgoing-copy-05cab29`, now
`/opt/zecswap/current`. At about 23:00 UTC the mainnet maker restarted on a build of `1faaf63`,
the `cc8e06e` code it ran with the fix, in
`/opt/zecswap-mainnet/releases/20261017-sweep-outgoing-copy-1faaf63`, now
`/opt/zecswap-mainnet/current`; it still reads funds the old way. On both networks the relayer
and issuer are the previous release's, byte for byte, and did not restart; no config changed.

Before changing anything, the deploy checked the staged checksums and, on each network, that the
release, its maker and the running maker were the recorded ones and that health answered 204.
After each restart it saw health 204 with the staged maker running, the maker's info naming its
network, chain and contract, and no warning or error in the next 45 seconds; both public gateways
then answered health and info the same.
[sepolia-sweep-outgoing-copy.json](../deployments/sepolia-sweep-outgoing-copy.json) and
[mainnet-beta.json](../deployments/mainnet-beta.json) record the checksums.

The app's own sweeps still carry the copy: Zcash SDK 3.2.1, which builds them, always keeps it.
Until the app sweeps each refund and reverse swap's ZEC to a never-used address, the maker can
read the address it goes to, today the wallet's receive address.

Rollback files are in `/var/backups/zecswap/20261017-sweep-outgoing-copy-05cab29` and
`/var/backups/zecswap-mainnet/20261017-sweep-outgoing-copy-1faaf63` (root-only):
`maker-config.toml` and `previous-current`. Point the network's current link back at the release
`previous-current` names and restart its maker; the stores didn't change.

## October 18 mainnet funds from one summary

At 03:21 UTC the mainnet maker restarted on testnet's own maker. That is the build of `05cab29`
testnet has run since October 17 (`7dba650b…`), whose Rust code is main's (`1e362db`), in
`/opt/zecswap-mainnet/releases/20261018-monitor-funds-05cab29`, now `/opt/zecswap-mainnet/current`.
It brings mainnet the funds read from one wallet summary (`1765bb6`,
[October 14](#october-14-testnet-inventory-and-monitor)), so both networks run the same maker. The
relayer and issuer are the previous release's, byte for byte, and did not restart; no config
changed.

Before changing anything, the deploy checked that testnet still ran this maker with health 204,
and that mainnet ran its recorded release and maker with health 204. Mainnet's monitor answered in
0.15, 0.006 and 1.08 seconds before the restart and 0.39, 0.46 and 0.013 after. With few swaps
listed it was already quick: the old way took a summary for each swap it listed, so its cost grew
with them. After the restart the deploy saw health 204 with the new maker running, its info naming
mainnet, chain 1 and the contract, and no warning or error in 45 seconds. The public gateway then
answered health and info the same. [mainnet-beta.json](../deployments/mainnet-beta.json) records
the checksums.

Rollback files are in `/var/backups/zecswap-mainnet/20261018-monitor-funds-05cab29` (root-only):
`maker-config.toml` and `previous-current`. Point `/opt/zecswap-mainnet/current` back at the
release `previous-current` names and restart `zecswap-mainnet-maker`; the store didn't change.

## October 18 deposits and a new test token

Deposits ([deposits.md](deposits.md)) move a deposit address's tokens on its EIP-3009
authorization, which the old test token lacks, so testnet has a new one. From `3bba12a`,
`deploy/testnet/deploy-deposits.sh` deployed on Sepolia, from `0x09ed…c3e5`:

| | Address | Block |
| --- | --- | --- |
| tUSD (`ZecSwap Test USD`, version `2`, 6 decimals, anyone may `mint`) | [`0x64c4a5cb09fe0b706d4cf24dc0a45fff275d727c`](https://sepolia.etherscan.io/address/0x64c4a5cb09fe0b706d4cf24dc0a45fff275d727c) | 11,944,315 |
| Deposits (Railgun's Sepolia proxy, no sanctions oracle) | [`0x58324e9a866c9690aca08169d3c67630e84c9019`](https://sepolia.etherscan.io/address/0x58324e9a866c9690aca08169d3c67630e84c9019) | 11,944,316 |

The swap contract stays: it keeps inventory per token, and each swap keeps its own.
[sepolia-deposits.json](../deployments/sepolia-deposits.json) records both deployments and the
switch.

At 23:41 UTC `deploy/testnet/switch-token.sh` (`120106d`) found no live swap, stopped the maker and
the relayer, and moved the maker's inventory. It withdrew the 956.670142 old tUSD to the maker's
account (`0xf51eab97…`, block 11,944,396), then minted 2,000 new tUSD and added them in
`0x8e60474c…`, `0xd871944f…` and `0x441c1832…` (blocks 11,944,397 to 11,944,399). Both configs now
name the new token. The relayer's gained `[deposits]` (`deploy/testnet/relayer-deposits.toml`: a
fee of 0.1 tUSD and 0.1%, priced from 1.7M gas when that costs more, a 2 tUSD minimum,
authorizations valid up to a day, eight on their way at once), and the gateway an allowance for
deposits of one a second, a burst of ten. The relayer runs a build of `120106d` (`d30cbcb4…`) in
`/opt/zecswap/releases/20261018-deposits-120106d`, now `/opt/zecswap/current`; the maker and issuer
are the previous release's, byte for byte. Both restarted at 23:42.

The relayer sends deposits from an account of its own,
`0xedc4c88a3f6ae8130a2c18babbabd8100cc1a446`: the terms name it, and wallets pin it. It funded
that account with eight deposits' gas at the day's fees, and took back from the other accounts
what they held beyond twice their float at those fees (0.0657 ETH in all).

Before changing anything, the switch checked that both configs named the old token and the swap
contract, both services ran, the staged relayer started, and the Deposits contract, read on the
relayer's RPC, took the new token, shielded into Railgun's proxy and screened no one; after
stopping the maker, that its store held no unsettled swap. After the restart the maker's health
answered 204, its info named the new token, and the relayer's terms named the Deposits contract,
token and account above, all the same through the public gateway. Then a fresh wallet's first
deposit address got 5 tUSD and `zecswap-cli deposit` shielded it through the gateway: the relayer
sent `0xd7667941…` (block 11,944,419, 1,663,324 gas at 0.02 gwei) 14 seconds after the CLI
signed, and kept 0.105 tUSD. The wallet's Railgun balance got 4.882763, which Railgun's wallet
SDK showed Spendable within five minutes.

The maker's account keeps the 956.670142 old tUSD it withdrew. Old-tUSD Railgun balances and
relayer fees stay where they are. App builds that pin the old token can no longer swap on testnet,
since the maker's info and the relayer's terms name the new one. zapp-dashboard pins the testnet
token in `lib/bridge/config.ts`; until a build pinning the new one deploys, it reports the maker's
and the relayer's token as unexpected.

Rollback files are in `/var/backups/zecswap/20261018-deposits-120106d` (root-only):
`maker-config.toml`, `relayer-config.toml`, `nginx-site` (its path in `nginx-site-path`),
`previous-current` and `maker.sqlite`. Rolling back moves the inventory back on chain:
1. Stop both services.
2. Run `withdraw-inventory all` under the current maker config.
3. Restore both configs and the nginx site, and reload nginx.
4. Run `add-inventory 956670142` under the restored config: the maker's account holds that old
   tUSD.
5. Point `/opt/zecswap/current` at the release `previous-current` names, and start both.

Deposits then stop. Their journal stays in `/var/lib/zecswap-relayer/deposits.sqlite`.

## October 19 a larger swap cap

At 02:15 UTC the maker refused a quote for 0.1 ZEC (116.686723 tUSD) as outside the quotable
range: `[pricing] max_units` let a swap be at most 20 tUSD, about 0.017 ZEC with ZEC near $1,180.
At 02:27, once no swap was live, the cap went to `200000000` (200 tUSD, about 0.17 ZEC) and the
maker restarted on the same release. Its health answered 204, its monitor showed the new cap, and a
quote for 116.686723 tUSD through the gateway asked 0.10070382 ZEC.

Its 1,988.63 tUSD covers nine swaps at the cap at once, and its 1.36 TAZ eight the other way.
Testnet accepts need no tokens and up to 20 swaps may wait on deposits, so ten abandoned ones at
the cap can hold all the tUSD until the maker refunds them, 15 minutes after each opens.

To roll back, restore `/var/backups/zecswap/20261019-swap-cap-200/maker-config.toml` (root-only)
to `/etc/zecswap-maker/config.toml` and restart the maker.

## October 19 mainnet deposits

At 03:42 UTC `deploy/mainnet/deploy-deposits.sh` (`63803a6`) deployed the Deposits contract on
Ethereum from the relayer's own account, `0xcBcb…017F`:
[`0xebc7508e20bd4467491ee7e7589d5c7d4c5e8cb6`](https://etherscan.io/address/0xebc7508e20bd4467491ee7e7589d5c7d4c5e8cb6)
in `0x08115a2e…` (block 26,224,004, 1,983,757 gas, 0.000137 ETH). It takes USDC into Railgun's
proxy and screens depositors against Chainalysis' oracle `0x40C5…C8fb`; the script read all three
back, and USDC's domain (`USD Coin`, `2`). [mainnet-deposits.json](../deployments/mainnet-deposits.json)
records it. Its source was verified on Etherscan on 2026-10-20 at 00:20 UTC, from the deployed
commit; its vault logic (`0x8123…3E39`) shows as verified too.

At 03:48 `deploy/mainnet/enable-deposits.sh` found no live swap and gave the relayer `[deposits]`
(`deploy/mainnet/relayer-deposits.toml`: a fee of $0.10 and 0.1%, priced from 950k gas when that
costs more, a 5 USDC minimum, authorizations valid up to a day) and a build of `63803a6`
(`bb8439d5…`) in `/opt/zecswap-mainnet/releases/20261019-deposits-63803a6`, now
`/opt/zecswap-mainnet/current`; the maker's and issuer's binaries are the previous release's,
byte for byte. Deposits go one at a time through Flashbots Protect,
`https://rpc.flashbots.net/fast?blockRange=10`, never the public mempool
([deposits.md](deposits.md)). The mainnet gateway's site, made from `deploy/nginx.conf` by
`deploy/mainnet/nginx-site.sed` (which `install.sh` now uses too), gained the deposits' allowance;
it differed from the live one in that alone. Only the relayer restarted, at 03:48:29. Deposits go
from `0x6ae40b34c32d3feaa0eef53d7e4724160b7a2135`, which the relayer topped up with 0.000294 ETH,
two deposits' gas at the fee cap.

The relayer's own USDC fees, 2 USDC, were all the USDC it could move, so for a first deposit the
minimum was 1 USDC. One went to a test wallet's first deposit address (`0xb00a6862…`), and
`zecswap-cli deposit --network mainnet` shielded it through the gateway in 19 seconds. Flashbots
landed `0xabdd88db…` in block 26,224,038, 11 seconds after the relayer sent it, and never showed
it to the public mempool (its status page: `INCLUDED`, `seenInMempool: false`); it burned 910,821
gas at 0.056 gwei. The relayer kept 0.106214 USDC, Railgun 0.25%, and the wallet's balance got
0.891552, which Railgun's wallet SDK (`balance.cjs`, on mainnet with `RAILGUN_NETWORK=mainnet`)
showed ShieldPending at 04:30 and Spendable by 06:54, screening cleared (the scan needs
10,000-block log queries, which the owner's Infura key serves and Alchemy's free tier doesn't). The test wallet's seed is in
`.testnet/deposit-test-wallet` on the owner's machine. The owner left the minimum at 1 USDC for
now; `deploy/mainnet/relayer-deposits.toml` holds the 5 USDC to go back to (`min_value =
5000000` in the relayer's config, then a restart of the relayer).

Rollback files are in `/var/backups/zecswap-mainnet/20261019-deposits-63803a6` (root-only):
`relayer-config.toml`, `nginx-site` and `previous-current`. Restore the config and the site,
reload nginx, point `/opt/zecswap-mainnet/current` at the release `previous-current` names, and
restart the relayer. Deposits then stop; their journal stays in
`/var/lib/zecswap-mainnet-relayer/deposits.sqlite`.

## October 20 windows contract

The windows contract has no locks: at every moment at most one side may reveal its share, by the
clock and the swap's stage alone ([reverse-flow.md](reverse-flow.md#windows-2026-10-20)). From
`0de4c06` (#17), `deploy/testnet/deploy-windows.sh` deployed it on Sepolia from `0x09ed…c3e5` at
23:58 UTC, with a 30-minute minimum window and a 1-hour late window, paying into Railgun's Sepolia
proxy:

| | Address | Block |
| --- | --- | --- |
| ZecSwap (windows) | [`0x459b0a69e0f736666b66a3ffe0fc17c11f05980d`](https://sepolia.etherscan.io/address/0x459b0a69e0f736666b66a3ffe0fc17c11f05980d) | 11,958,709 |

It used 27,474,590 gas, and read back both windows and the proxy, and no `LOCK_DURATION()`.
[sepolia-windows.json](../deployments/sepolia-windows.json) records it and the switch; the
terms-hash contract's manifest moved to `deployments/retired/`.

At 00:00 UTC on October 21 `deploy/testnet/switch-windows.sh` checked the maker's timing against
the windows, found no live swap, stopped the maker and the relayer, and moved the maker's
inventory: it withdrew the 2,188.628126 tUSD from the terms-hash contract (`0x8409b628…`, block
11,958,722) and added them to the windows contract in `0x105813d6…` and `0x68b8e6b6…` (blocks
11,958,723 and 11,958,724). Both configs name the new contract, and the maker started on a fresh
store, keeping its keys, root secret, wallets and spent tokens. Both run a build of `0de4c06` in
`/opt/zecswap/releases/20261020-windows-0de4c06`, now `/opt/zecswap/current`; the issuer is the
previous release's, byte for byte. Both started at 00:01:13. The maker's timing stays as it was: t0
45 minutes after its open, t1 an hour after t0, an unpaid swap cancelled 5 minutes before t0, and
no reveal sent with under 2 minutes left in its window.

After the restart the maker's health answered 204, and its info and the relayer's terms named the
new contract, all the same through the public gateway. zapp-dashboard reads both contracts'
records and both kinds of maker policy since `866df80`, and pins the new contract since `6b510a3`
(`BRIDGE_TESTNET_CONTRACT` too); both are deployed, not yet pushed.

Then `zecswap-cli swap --relayer` swapped 2 tUSD through the gateway (`0x6f522efa`): accepted at
00:05:43, readied by the maker at 00:08:36 once the 179,155-zatoshi deposit had its two
confirmations, claimed through the relayer at 00:11:00, twelve blocks after the ready as a claim
before `t0` waits, and paid out at 00:11:12, 1.9 tUSD into the wallet's Railgun balance
(`0x136de610…`, block 11,958,774). The maker then swept the deposit (`ba68939c…`).

At 01:11:58 UTC on October 21, with no live swap, the maker and the relayer moved to a build of
`e7c5467` in `/opt/zecswap/releases/20261021-windows-e7c5467` (`previous-current` in
`/var/backups/zecswap/20261021-windows-e7c5467`), the issuer the previous release's, byte for
byte. Every reveal is judged there as the third audit's port spec decides its 97 cases: a claim
before `t0` waits for 12 blocks above its ready, where the first build took 11; a reverse claim
takes a ready 64 blocks deep as final when the finalized block doesn't hold it yet; and the
relayer never readies an escrow after a refund it sent. A second 2 tUSD swap through the gateway
(`0x002ed897`) then ran on it: readied at 01:17:36 (block 11,959,102), claimed at 01:20:24
(block 11,959,115), sent once 12 blocks stood above the ready, and paid out 1.9 tUSD into the
wallet's Railgun balance (`0xeb876c2c…`).

App builds that speak locks can no longer swap on testnet: the relayer has no lock routes, and
the maker's info and the relayer's terms name the new contract. Mainnet stays on the lock contract
and its release.

Rollback files are in `/var/backups/zecswap/20261020-windows-0de4c06` (root-only):
`maker-config.toml`, `relayer-config.toml`, `previous-current`, and the retired store as
`retired-maker.sqlite` with its backup `maker.sqlite`. Rolling back moves the inventory back on
chain:
1. Stop both services.
2. Run `withdraw-inventory all` under the current maker config.
3. Restore both configs and the retired store as `/var/lib/zecswap-maker/maker.sqlite`.
4. Point `/opt/zecswap/current` at the release `previous-current` names.
5. Run `add-inventory 2188628126` under the restored config, and start both.

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
   The relayer serves one token and one maker: set its `token` and `maker`. To have it send
   wallets' private Railgun sends and withdrawals for a fee note, give it `[railgun_sends]` and
   its own Railgun wallet's seed in `RELAYER_RAILGUN_SEED` ([railgun-sends.md](railgun-sends.md));
   its journal belongs in its state directory.
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

## Switching to mainnet

One setting picks the network: `network = "testnet"` or `"mainnet"` in the maker's, the
relayer's and the issuer's configs. `crates/zecswap-api/src/network.rs` holds what each network
fixes, and each service refuses to start on anything of another network:

- The maker and the relayer refuse an EVM RPC on another network's chain (mainnet: Ethereum,
  chain 1; testnet: Sepolia, Base Sepolia or a local anvil), a contract paying into a Railgun
  other than the network's own there (mainnet `0xFA7093CD…`, Sepolia `0xeCFCf3b4…`), and on
  mainnet a token other than USDC (`0xA0b86991…`).
- The maker refuses a `sweep_to` of another Zcash network, and its wallets a lightwalletd serving
  another (each asks its server once). `lightwalletd` defaults to the network's.
- The relayer's fundings go through Railgun's own Relay Adapt on its chain, the default for
  `[reverse_funding] relay_adapt`; another one is refused.
- The issuer on mainnet refuses `insecure-test`, whatever `allow_insecure` says.
- `add-inventory --mint` is refused on mainnet, and the CLI's `--network` picks its lightwalletd.
- The relayer on mainnet refuses to start without `[reveals] private_rpc`, below.

Claims and reverse refunds reveal a share in their calldata. One that landed after its window
shut would revert with the share public, and the other side, whose window opens next, could take
both halves of the swap. So on mainnet the relayer sends them through Flashbots Protect, never the
public mempool: `[reveals] private_rpc`, staged in `deploy/mainnet/relayer.toml` as
`https://rpc.flashbots.net?hint=hash&builder=Titan&builder=Quasar&builder=Eureka&blockRange=10`.
Protect lands a transaction only if it succeeds and drops one it couldn't land within its
`blockRange`. No node shows one until it mines, so the relayer sends them one at a time from its
claims account, holding the account's turn until each mines or 5 blocks past its range (15 here)
go by without it, and answers a dropped one `503`, to post again; a restarted relayer sends none
for those 15 blocks, since one it sent before may still land. Only the broadcasts go to Protect,
which limits reads to a few hundred an IP every five minutes: reads, nonces, simulations and
receipts stay on `evm_rpc`. The relayer refuses Protect's `/fast`, which shows TEE searchers whole
transactions (failed ones too, five minutes on); any `hint` but `hash` alone, since the default
hints share logs and `Claimed` logs the share; and any parameter but `hint`, `blockRange`,
`builder`, `refund`, `originId` and `auctionTimeout`, such as `canRevert` or `useMempool`.
Flashbots' own builder, BuilderNet, always gets the transaction, but it built 14.3% of blocks in
the day to October 19 (relayscan.io): alone it would land a claim in about 7 blocks and miss a
10-block range one time in five. Naming Titan (51.6%), Quasar (21.4%) and Eureka (7.5%), as
Flashbots' builder registry spells them (`builder-registrations.json` in github.com/flashbots/dowg),
covers about 95%; each sees the whole transaction, as it must to include it, and nothing more is
shared. Confirm the names on the first mainnet claim: Protect's status page for the transaction
shows where it went. Names no builder answers to leave it to BuilderNet alone: slower, still
private. Testnet keeps the public mempool, since Flashbots' Sepolia relay has included nothing
since block 11,856,337. Wherever a reveal goes, it is broadcast only while its window still has
`claim_margin` left once its turn comes, read with the newest block and judged by the later of its
time and the wall clock; one whose window ran under it while it waited is refused with `400`,
nothing sent, as on arrival. A reveal sent privately is never simulated, as a simulation would show
the RPC its share: it carries a fixed gas limit instead. A send through the public mempool, a
testnet reveal among them, goes again at the same nonce at the fees then when three blocks pass
without it. The maker's own reveals, its cancels and
refunds, go through Protect too once its config has `[reveals] private_rpc`, which
`deploy/mainnet/maker.toml` stages commented out: the owner's call.

Swaps use the Ironwood pool, which NU6.3 brought (mainnet height 3,428,143; zec.rocks served
mainnet on NU6.3's branch `0x37A5165B` on October 12), so a maker on the pinned crates runs on
mainnet. They schedule NU7 on testnet alone: before NU7 activates on mainnet (its height is due
to be chosen on October 20), the maker and the app must move to crates that schedule it there,
or their Zcash transactions are refused, as testnet's were on October 4.

A mainnet deployment also needs its own: a contract deployed with `RAILGUN` set to the mainnet
proxy, and the `MIN_WINDOW` and `LATE_WINDOW` chosen for it (`scripts/deploy-contract.sh`); the maker's and
relayer's `contract`, `token`, `maker` and RPC URLs ([deploy/mainnet/](../deploy/mainnet/)
holds the beta's configs); mainnet ETH and USDC; secrets of its own
(`scripts/mainnet-secrets.sh`, whose BIP-39 phrases restore the maker's ZEC inventory in any
Zcash wallet and the relayer's fees in any Railgun wallet); its services, which
`deploy/mainnet/install.sh` puts beside the testnet's on the droplet under names, users, folders
and ports of their own (`zecswap-mainnet-*`, 127.0.0.1:8797-8799 behind nginx on 8090); the
issuer's production packages and signing digests; a Worker of its own
(`deploy/worker/wrangler.mainnet.jsonc`) on a VPC service of its own through the droplet's
tunnel; mainnet observability labels
([observability.md](observability.md)); deposits counted at 2 confirmations, the owner's choice
for the beta's small swaps (the wallets' default is 10); and the dashboard's mainnet environment.
Funding sponsorship has no aggregate
budget, which matters more on mainnet.

Funds leave a mainnet deployment by these routes alone. `max_awaiting_deposit = 0` and a restart
refuse every new swap while the watchtower settles the open ones; `zecswap-maker
withdraw-inventory all --to <address>` then moves the USDC no swap holds; the maker's Zcash phrase
restores its ZEC inventory in a Zcash wallet, with the maker stopped; forward swaps' ZEC is swept
to `sweep_to` as they settle; and `zecswap-relayer drain-lanes --to <address>`, with the relayer
stopped, empties its sending accounts. The maker's and relayer's own ETH leave with their keys.

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
`wrangler.jsonc` (a template: the real ids sit in the gitignored `wrangler.local.jsonc`, which `npm run deploy` uses), and the Worker needs the maker's `MAKER_TICKET_SECRET` as its own secret
(`npx wrangler secret put TICKET_SECRET`). The gateway streams request bodies unchanged, disables
caching and does not retry financial requests. It forwards only the `Authorization`,
`Content-Type` and `Content-Length` headers, so it passes on nothing else a client sends, nor the
client address Cloudflare attaches to the request.

Each accept answers with a ticket for its swap: the swap id and an HMAC-SHA256 of it under
`MAKER_TICKET_SECRET`, a secret of its own that leads to none of the maker's keys. Wallets send it
in the `ZecSwap-Ticket` header of every relayer request for the swap. The Worker checks it with
WebCrypto, never asking the origin, and only then adds a `ZecSwap-Swap` header naming the swap; it
drops both headers from what a client sends.

Every request reaches nginx from `cloudflared` on loopback, so nginx can't tell callers apart,
and limits are never per IP. `deploy/nginx.conf` gives each kind of request its own allowance
instead, so a flood of one kind never refuses another:

| Requests | Allowance |
| --- | --- |
| A swap's relayer requests (fundings, readies, claims, payouts, refunds, rescues) with a ticket that checks out | 1 a second for each swap; up to 10 more wait their turn |
| The same requests without one | 2 a second shared; up to 20 more wait their turn |
| Quotes | 2 a second, bursts of 30 |
| Accepts | 10 at a time; 2 a second, bursts of 20 |
| Private Railgun sends and withdrawals: `railgun/transact` | 2 a second, bursts of 20 |
| The issuer's challenges and tokens | 2 a second, bursts of 20 |
| Reverse swap status, which reads the chain on every request | 2 a second, bursts of 20 |
| Everything else | 10 a second, bursts of 50 |

Each swap's status also has its own allowance, of one a second with bursts of ten, so an app
polling its swap too fast slows only that swap. Relayer requests without a ticket share their
small allowance while apps that send none are about; once every app sends tickets, setting
`$zecswap_tickets_required` to 1 in `deploy/nginx.conf` refuses them with `403` instead. Past an
allowance nginx answers `503` with `Retry-After` and the gateway's `unavailable` error, which
clients take as a passing failure; the services' own `503`s pass through unchanged. It never
answers `429`, which from the issuer means a device's tokens for the day are spent.
`zecswap-cli` waits such a `503` out and sends the request again, for 90 seconds in all.

A quote reserves nothing: it costs the maker a row, and at most two chain reads, its gas price
and inventory readings lasting a minute and half a minute. An accept spends the
maker's gas on its `open` and answers once the open lands, and the maker's opens queue one a
block, so ten accepts at a time finish within the three minutes a client waits for one; past
them, an accept is turned away at the gateway before it costs anything, its quote still good.
An accept paid with a token is posted twice, the first answered with the token's challenge.

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
validation and gas caps, and there is no aggregate sponsorship budget. A flood of relayer
requests without tickets fills only their shared allowance, never a ticketed swap's. Tickets are
only as scarce as accepts, though: each accept gives one, and so each token spent on one, which
the issuer's attestation bounds. Rely on the cap and the tokens, not on the allowances. Keep the existing forward ordering:
depositing ZEC before escrow exists would remove its contract-backed recovery path.

### RPC use

The maker and the relayer read Ethereum through the RPC URL in their configs, and Alchemy caps
what an account spends a month (30M compute units on its free tier) whichever of its keys spends
it: give a deployment an account of its own, apart from development's and the apps', or a
pay-as-you-go plan. Each service logs its RPC when it starts, by its host and a fingerprint of
its key (the first eight hex digits of the key's SHA-256, never the key), and every hour the
requests it sent by method, with the compute units Alchemy bills for them
(`RPC requests in the last period`); each monitor exports the same since its start as `rpc`.

| What | Reads | Compute units |
| --- | --- | --- |
| Maker watchtower, every 15 s | With no live swap, one `eth_chainId`, billed nothing. Otherwise the latest block, every live swap in one call, and those settling a few blocks deep in another. A swap whose claim or refund a finalized block holds is never read again | 0 idle; 50 to 75 a pass, however many swaps |
| Maker Zcash worker | The watchtower's reads; reverse swaps read the same way | 0 without reverse swaps |
| Transaction observer | Each block's events once, in ten-block windows, every 15 s while swaps are live and every 90 s otherwise; the newest twelve again every 2 minutes, or 10 | about 85k a day idle |
| Quotes and reverse status | The gas price once a minute, the inventory every 30 s; status from the passes' reads | little, however often asked |
| Relayer lanes | Every lane's balance in one call, after each of its sends and every 2 minutes | about 35k a day |
| Relayer fees | The gas price, once a minute at most, however often terms are read | at most 30k a day |

Idle, the two spend about 0.15M compute units a day; a swap about 20k more, beside its own
transactions.

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

Each swap sends two messages, queued in the same SQLite transaction as the state change they
report: one when its quote is accepted and one when it ends. Each names the network, the
direction (ZEC → USDC or USDC → ZEC) and the exact amounts, gives the time and the first eight
hex digits of the swap ID (the dashboard's search finds a swap by them), and ends with one
link. The last one says how the swap ended and how long it took: completed, refunded,
cancelled (no ZEC arrived) or expired (its escrow never opened or was never funded, or the
maker sent no ZEC in time), with a line on why unless it completed. It links the claim,
refund or payout that settled the swap on Etherscan, or the dashboard if the maker has not
indexed one yet. When Railgun sends a payout back and a rescue shields it again, the rescue
sends one more message and is its link. Nothing else about a swap sends a message: deposits,
confirmations, readies and the maker's sweeps are on the dashboard. Messages never carry the
contract, the maker's address or block numbers, and the notification worker never asks a
price feed.

The relayer sends one message for each Railgun send it broadcasts for a wallet, once the send
mines, queued in its journal in the same write as the send's cost. A private send (0zk to 0zk)
gives the fee it paid the relayer, as its amount is private. An unshield (USDC out of Railgun
to a public address) gives the amount it took out, before Railgun's own unshield fee, and the
relayer's fee. A send that reverted says that nothing moved and the relayer paid the gas. Each
ends with the time it mined and the transaction on Etherscan, and none names an address. Give
the relayer the maker's pair with a drop-in
`/etc/systemd/system/zecswap-relayer.service.d/telegram.conf` naming the same
`EnvironmentFile=/etc/zecswap-maker/telegram.env` (systemd reads it as root), then restart it.
Its `sending Railgun transactions` log line shows `alerts=true`. A relayer without
`[railgun_sends]` sends nothing.

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
An error sends one message once it has lasted five minutes, and no more until a pass
succeeds; a pass that succeeds also starts its five minutes again, so a blip of the RPC or
lightwalletd sends nothing. The errors are a swap the watchtower cannot advance, failing or
panicking watchtower passes, and a failing Zcash sync. Protect `maker.sqlite` backups since
they also contain public swap alert text.

`[gas_alerts]` watches the ETH of the account its service sends from: in the maker's
configuration the maker's own, in the relayer's the relayer's own (`RELAYER_PRIVATE_KEY`'s, which
tops up its other sending accounts; it needs `[railgun_sends]`, whose journal queues the
messages). Only an account's own sends spend its ETH, so it is read when the service starts and
each time one of its sends finishes, mined or failed, and not otherwise. It sends one message
when the account drops under `low_wei`, with its balance and address, and one when it is back at
`recovery_wei` or more; a first healthy reading sends nothing. A top-up comes from elsewhere and
shows in none of its sends, so while the account is low it is also read every `interval_seconds`
(60 to 3600) until it is topped up. Both services take the same section:

```toml
[gas_alerts]
low_wei = 20000000000000000       # 0.02 ETH
recovery_wei = 50000000000000000  # 0.05 ETH
interval_seconds = 300
```

Each monitor's `gasAlerts` names the account it watches.

`GET /v1/monitor` includes `notifications`: enabled state, pending count, oldest
pending time, last acknowledged delivery time, and a sanitized delivery error.
It never includes the bot token or destination ID. The dashboard displays this as
the Telegram alerts health check. Mainnet uses the same code: deploy its own maker
with separate configuration/data. The bot/destination environment pair is its switch: give
the mainnet maker and relayer the pair (the testnet's chat will do, since every message names
its network and links its own chain's explorer and dashboard), and each a `[gas_alerts]` for
its own account, then run `telegram-test`; without the pair neither sends anything.
Deduplication IDs include the deployment.

Ethereum transaction references are indexed from the configured settlement contract's
confirmed events, including transactions sent by the app or relayer. A separate
read-only task scans ten-block RPC windows without locking the wallet or fetching
prices. Recent blocks are scanned first; historical swaps are backfilled from their
earliest accepted quote. SQLite stores the cursor and public event metadata under
the network/chain/contract/maker scope. Each block is read once as the cursor passes it,
and the latest twelve again every two minutes while swaps are live (every ten otherwise),
to remove events a shallow reorganization orphaned; deeper reorgs require an operator
rescan. Confirmation depth follows `reverse.evm_confirmations`, or two when
reverse swaps are disabled. Confirmed does not mean finalized.

A rescue is the only event with a Telegram message of its own; every other event reaches
Telegram only through its swap's start and end. Historical backfill never replays old
events, and alert deduplication and cursor advancement are atomic. `/v1/monitor` exports
`transactions` indexing status and each swap's `evmTransactions` public hash/block/event
list, never log payloads, revealed shares, RPC credentials or Telegram configuration.

### Dashboard monitoring

Live USD pricing is enabled by `[pricing.market]` in the maker configuration:

```toml
[pricing.market]
token_decimals = 6
refresh_seconds = 60
max_age_seconds = 300
providers = ["coinmarketcap", "alchemy"]
```

`providers` is required and ordered: each refresh asks the first, and only while it fails the
next, so quotes keep their price through one provider's outage. Each listed provider needs
its key in the maker's protected environment file: `ZCASH_CMC_KEY` for CoinMarketCap,
`ALCHEMY_API_KEY` for Alchemy's Prices API (the key of an Alchemy app; it is sent as a bearer
header, never in a URL). Neither is ever a dashboard variable or part of a browser response.
Quote and monitoring requests fetch ZEC and USDC USD prices together (CoinMarketCap by ID,
1437 and 3408; Alchemy by symbol), using a shared 60-second cache and a single in-flight
request. The same request carries ETH (CoinMarketCap ID 1027) for gas values alone: missing
or stale, it is left out and never touches quotes. The monitor's `pricing.source` names the
provider behind the current price, and `pricing.providers` each provider's last error.
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

### Each swap pays its own network cost

The spread is a share of the swap, but the maker's gas and Zcash fee are the same whatever its
size, so a small swap's spread can't cover them. With `[pricing.costs]` every quote charges them
on top of the spread, at the chain's gas price when it is made (read at most once a minute) and
the quote's own ZEC, USDC and ETH prices:

```toml
[pricing.costs]
forward_gas = 450000      # ZEC to USDC: the maker's open and ready (412k on Sepolia)
reverse_gas = 400000      # USDC to ZEC: its claim (with the lock contract's claim lock, 362k on Sepolia)
zcash_fee_zat = 15000     # the forward sweep, or the reverse deposit (10,000 so far)
margin_bps = 2000         # for gas prices that rise before the sends land
```

A ZEC to USDC quote asks that much more ZEC for the same USDC; a USDC to ZEC quote pays that much
less ZEC, and a swap too small to carry its cost gets no quote. Quotes say what they charged
(`networkCost`, token base units), and the economics export keeps it with the quote's prices
(`quote.networkCost`, `quote.gasPriceWei`). It needs `[pricing.market]`, which then needs ETH's
price as well: quotes pause while ETH's price or the gas price can't be read, as they do for ZEC.
Measure the gas on the chain served, from the dashboard's costs by operation: Sepolia reprices
new state, so its budgets overstate Ethereum's.

The sends pay that gas price. The maker's and the relayer's contract calls tip what the node
suggests, the tip its gas price is made of, under a cap of twice the next block's base fee on
top; Railgun sends and fundings pay the gas price itself, or their proof's minimum if higher.
alloy's own estimate tips the median of the last ten blocks' tips instead: Sepolia's bots tip a
flat gwei, so it ran to a thousand times what quotes charged, and a ZEC to USDC swap whose quote
charged $0.001 of gas paid $0.95 to open its escrow.

The relayer's fees follow gas the same way. With `providers` at the top of its config, a
payout's fee is the larger of `fee` and `fee_gas` (the claim and the payout; with the lock
contract's claim lock, 1.65M on Sepolia) at the gas price and ETH price when it is quoted, `fee_margin_bps` on top;
`[reverse_funding]` has its own `fee` and `fee_gas` (the funding and its ready: 2.06M). The terms
quote both, with `feeExpiresAt`: the relayer takes any fee at or above the lowest it quoted in
the hour before, so the fee a wallet shows when a swap starts is the fee its claim pays. Where it
can't price gas, it takes the floor rather than hold a claim up.

`GET /maker/v1/monitor` is a read-only operations export for `zapp-dashboard`,
`GET /maker/v1/monitor/swaps/{id}` the same record for any one swap, old or new, and
`GET /maker/v1/monitor/economics?since=<unix seconds>` what each swap accepted since then
(30 days back by default, newest first, at most 1,000) earned and cost. All three are
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

The economics export (`schemaVersion` 1) gives each swap its amounts, the market prices its
quote was made at (`quote`: `source` is the provider, or `live`/`history` for a swap accepted
before quotes kept their prices, valued afterwards), every Ethereum transaction on it and
the maker's Zcash sends on it. A transaction is listed if one of the swap's events was in it
or the maker sent it for the swap, a reverted send included: its sender, success, gas used,
effective gas price, the escrow token it paid a relayer (moved to its sender, or for a sponsored
funding, to the relayer's own account while its funding account sends it) and ETH/USD at
its block's time. A Zcash send (`sweep`, reverse `deposit`, or `recovery` after a refund) has
its exact fee and ZEC/USD when it was sent. Prices are the maker's own when it saw the event
within five minutes (`live`), else Alchemy's nearest five-minute candle within a quarter hour
(`history`; only with `alchemy` among the providers): its series has gaps of a candle or two. A
value that is neither stays missing, and a time Alchemy has no price for is asked about again an
hour on. The maker's transaction observer records all of this as it goes, a bounded amount a
pass, reading each receipt once; swaps from before this release are filled in the same way.

The endpoint reads nothing from the chain: contract states, USDC and ETH balances are the
dashboard's to read, on its own RPC, so monitoring never spends the RPC the watchtower needs.
It never syncs the wallet, generates proofs, sends transactions, or writes inventory. A busy
wallet uses the last captured inventory reading and sets `walletBusy`; individual wallet
readings remain unknown while busy.

The relayer's `GET /relayer/v1/monitor` (`RELAYER_MONITOR_TOKEN`) counts, for each operation
since it started, the transactions sent, the requests refused and those that failed (by kind:
`reverted`, `unconfirmed` or `internal`, never a message), and the gas and wei its sent
transactions burned, from which the dashboard works out how long its ETH lasts. With
`[railgun_sends]`, its `GET /relayer/v1/monitor/sends` lists each private send and withdrawal it
paid the gas of, with what it burned and the fee it was paid ([railgun-sends.md](railgun-sends.md)).
The issuer's `GET /issuer/v1/monitor` (`ISSUER_MONITOR_TOKEN`, in
`/etc/zecswap-issuer/secrets.env`) shows the day's token totals, its refusals by reason and its
status list's age. Both are closed while their token is unset and take tokens of at least 32
characters. All three
monitors share a gateway allowance of their own (one a second, a burst of ten), apart from the
apps'.

The dashboard validates the deployment and token precision before formatting USDC.
The pinned test token uses explicitly configured six-decimal USDC units (the one before October
18 had no `decimals()`); mainnet has no default token precision.
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
cancellation intent and a mark that it sent a swap's ready, each recorded before the send: a swap
the maker began to cancel is never made ready, and one it made ready never cancelled, whatever a
restart or a reorganisation shows.
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
Deposits and escrow spends send no Telegram message. No additional credentials or price
cron are needed.
