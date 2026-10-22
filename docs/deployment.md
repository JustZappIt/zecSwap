# Deployments

Two deployments run on the DigitalOcean Droplet at `<droplet ip>`, each behind a Cloudflare Worker
that reaches the host through a Workers VPC Service and a named Cloudflare Tunnel. Their SQLite
stores live on the Droplet's persistent disk, which is why they run on a host and not in Cloudflare
Containers, whose disks are ephemeral.

**Testnet** (Ethereum Sepolia and the Zcash testnet) runs the windows contract
[`0x459b0a69e0f736666b66a3ffe0fc17c11f05980d`](https://sepolia.etherscan.io/address/0x459b0a69e0f736666b66a3ffe0fc17c11f05980d)
([October 20](#october-20-windows-contract)), paying in tUSD
`0x64c4a5cb09fe0b706d4cf24dc0a45fff275d727c`, with deposits through
`0x58324e9a866c9690aca08169d3c67630e84c9019`
([October 18](#october-18-deposits-and-a-new-test-token)). Its maker, relayer and token issuer run
from `/opt/zecswap/current`. [sepolia-windows.json](../deployments/sepolia-windows.json) records the
contract and every release since; [sepolia-deposits.json](../deployments/sepolia-deposits.json)
records the token and the Deposits contract.

| API | Base URL |
| --- | --- |
| Maker | `https://<testnet gateway>.workers.dev/maker` |
| Relayer | `https://<testnet gateway>.workers.dev/relayer` |
| Token issuer | `https://<testnet gateway>.workers.dev/issuer` |

**Mainnet beta** (Ethereum and Zcash mainnet) runs the lock contract
[`0x4708D7d36992b7e33Ad97a1ab0A31c3ac6C4fe6D`](https://etherscan.io/address/0x4708D7d36992b7e33Ad97a1ab0A31c3ac6C4fe6D),
with a 30-minute lock, paying in USDC, with deposits through
[`0xebc7508e20bd4467491ee7e7589d5c7d4c5e8cb6`](https://etherscan.io/address/0xebc7508e20bd4467491ee7e7589d5c7d4c5e8cb6)
([October 19](#october-19-mainnet-deposits)). Its services, `zecswap-mainnet-*`, run from
`/opt/zecswap-mainnet/current`, a release from before the windows port. Builds of main refuse the
lock contract, so mainnet moves only to a windows contract of its own
([Switching to mainnet](#switching-to-mainnet)). [mainnet-beta.json](../deployments/mainnet-beta.json)
and [mainnet-deposits.json](../deployments/mainnet-deposits.json) record it.

Each network's maker and relayer share this host, under separate Unix users and keys. That is not
independent operation: in production the relayer must be run apart from the maker, as the
protocol requires.

## Testnet as it runs

- **Services.** `zecswap-maker` listens on `127.0.0.1:8787`, `zecswap-relayer` on `8788` and
  `zecswap-issuer` on `8789`, behind nginx on `127.0.0.1:8080` (`deploy/nginx.conf`), with
  `zecswap-tunnel` and `zecswap-issuer-status.timer`. Configs are in `/etc/zecswap-maker`,
  `/etc/zecswap-relayer` and `/etc/zecswap-issuer`, state in `/var/lib/zecswap-maker`,
  `/var/lib/zecswap-relayer` and `/var/lib/zecswap-issuer`, releases in
  `/opt/zecswap/releases/<release>`, and each release's rollback files in
  `/var/backups/zecswap/<release>` (root-only). Secrets live in root-only environment files beside
  the configs, never in them: each service's `secrets.env`, the maker's `monitor.env` for its
  monitor token, and `/etc/zecswap-maker/telegram.env` for Telegram's pair.
- **Accounts.** The maker sends from `0x2bac02B5032e9092493814c705F156B49E288922`. The relayer sends
  from `0xd9633572041886Fa7584a2E12F36C8C7f1126412`, which also tops up its sending accounts, and
  deposits from `0xedc4c88a3f6ae8130a2c18babbabd8100cc1a446`. Its Railgun wallet, which takes the
  fee notes of private sends, is `0zk1qy25r4x4…xpwljug`. Both services log their RPC by host and key
  fingerprint ([RPC use](#rpc-use)).
- **Swaps.** A swap is at most 200 tUSD (`[pricing] max_units = 200000000`), at most 20 wait on
  deposits at once (`max_awaiting_deposit`), and deposits count after 2 Zcash confirmations. A
  swap's `t0` is 45 minutes after its open and `t1` an hour after `t0`. One not paid in full is
  cancelled 5 minutes before `t0`, one with no deposit 15 minutes after its open, and no reveal is
  sent with under 2 minutes left in its window. Abandoned swaps hold their tUSD until then.
- **Relayer fees.** A payout keeps at least 0.1 tUSD, a private send 0.5 tUSD, a sponsored reverse
  funding 0.25 tUSD and a deposit 0.1 tUSD and 0.1%, each more when its gas costs more
  ([Each swap pays its own network cost](#each-swap-pays-its-own-network-cost)). Reverse fundings go
  through Railgun's Relay Adapt `0x7e3d929ebd5bdc84d02bd3205c777578f33a214d`, capped at 4M gas and
  20 gwei.
- **Tokens are off.** Since 2026-10-14 04:42 UTC the maker asks for no Privacy Pass tokens, the
  owner's call, so any phone, emulator or the CLI can swap. Its `[tokens]` table is commented out,
  and `/etc/zecswap-maker/config.toml.bak-20261013-tokens` holds the config with it. To ask for
  tokens again, uncomment the table and restart the maker. App builds that pin the keys work either
  way, since they send a token only when asked. The issuer still runs under `android-key`
  ([Token issuer attestation](#token-issuer-attestation)), with its key in
  `/etc/zecswap-issuer/key.pem`; the maker's return key is `/etc/zecswap-maker/return-key.pem`, its
  spent tokens are in `/var/lib/zecswap-maker/spent-tokens.sqlite`, and
  [sepolia-tokens.json](../deployments/sepolia-tokens.json) lists both public halves, which the app
  pins.

## October 20 windows contract

The windows contract has no locks: at every moment at most one side may reveal its share, by the
clock and the swap's stage alone ([reverse-flow.md](reverse-flow.md#terms-and-the-contract)). At 23:58
UTC, from `0de4c06` (#17), `deploy/testnet/deploy-windows.sh` deployed it on Sepolia in block
11,958,709, with a 30-minute minimum window and a 1-hour late window, paying into Railgun's Sepolia
proxy. It read both windows and the proxy back, and found no `LOCK_DURATION()`.

At 00:00 UTC on October 21 `deploy/testnet/switch-windows.sh` checked the maker's timing against
the windows, waited for no live swap, stopped the maker and the relayer, and moved the maker's
2,188.628126 tUSD of inventory from the terms-hash contract to this one (blocks 11,958,722 to
11,958,724). Both configs name the new contract. The maker started on a fresh store, keeping its
keys, root secret, wallets and spent tokens. The services have since moved to three later builds of
main, each time with no swap live and the issuer byte for byte the same:

| Since (UTC) | Release | What it brought |
| --- | --- | --- |
| Oct 21 00:01 | `20261020-windows-0de4c06` | The windows contract |
| Oct 21 01:11 | `20261021-windows-e7c5467` | Reveals judged as the port spec's 97 cases decide: a claim before `t0` waits for 12 blocks above its ready, a reverse claim takes a ready 64 blocks deep as final, and the relayer never readies after a refund it sent |
| Oct 21 03:22 | `20261021-wire-fields-a28ab10` | `contractKind`, `minWindow` and `lateWindow` in the relayer's terms and the maker's info, and `lateWindow` in a forward accept |
| Oct 22 02:44 | `20261021-held-claims-47650c6`, now `/opt/zecswap/current` | The relayer alone: it holds forward claims posted to `/v1/claim/hold` until their window opens ([Held claims](#held-claims); its config gained `[held_claims]`), a claim before `t0` waits for 2 blocks above its ready, not 12, and the gateway counts the new route among a swap's requests |

A swap through the public gateway ran end to end on each of the first three builds (`0x6f522efa`,
`0x002ed897`, `0xd85e3cd3`), paying into the wallet's Railgun balance; the manifest lists their
transactions. The held-claims build has had none yet: through the gateway its new route answered
`422` to an empty body, and its monitor shows no claim held. A
stress run from 01:35 to 02:20 UTC found every limit answering as designed. zapp-dashboard reads both
contracts since `866df80` and pins this one since `6b510a3`, as does `BRIDGE_TESTNET_CONTRACT`. App
builds that speak locks can no longer swap on testnet: the relayer has no lock routes, and the
maker's info and the relayer's terms name the new contract.

To undo an upgrade, point `/opt/zecswap/current` at the release in
`/var/backups/zecswap/<release>/previous-current` and restart the maker and relayer. Undoing the
switch moves the inventory back on chain, from the files in
`/var/backups/zecswap/20261020-windows-0de4c06`: `maker-config.toml`, `relayer-config.toml`,
`previous-current`, and the retired store as `retired-maker.sqlite` with its backup `maker.sqlite`.
1. Stop both services.
2. Run `withdraw-inventory all` under the current maker config.
3. Restore both configs, and the retired store as `/var/lib/zecswap-maker/maker.sqlite`.
4. Point `/opt/zecswap/current` at the release `previous-current` names.
5. Run `add-inventory` with the amount withdrawn under the restored config, and start both.

## October 18 deposits and a new test token

Deposits ([deposits.md](deposits.md)) move a deposit address's tokens on its EIP-3009
authorization, which the earlier test token lacked, so testnet got a new one. From `3bba12a`,
`deploy/testnet/deploy-deposits.sh` deployed on Sepolia:

| | Address | Block |
| --- | --- | --- |
| tUSD (`ZecSwap Test USD`, version `2`, 6 decimals, anyone may `mint`) | [`0x64c4a5cb09fe0b706d4cf24dc0a45fff275d727c`](https://sepolia.etherscan.io/address/0x64c4a5cb09fe0b706d4cf24dc0a45fff275d727c) | 11,944,315 |
| Deposits (Railgun's Sepolia proxy, no sanctions oracle) | [`0x58324e9a866c9690aca08169d3c67630e84c9019`](https://sepolia.etherscan.io/address/0x58324e9a866c9690aca08169d3c67630e84c9019) | 11,944,316 |

At 23:41 UTC `deploy/testnet/switch-token.sh` (`120106d`) waited for no live swap, stopped the maker
and the relayer, withdrew the maker's 956.670142 old tUSD to its account, where it stays, and minted
and added 2,000 of the new one. Both configs name the new token. The relayer gained `[deposits]`
(`deploy/testnet/relayer-deposits.toml`: a fee of 0.1 tUSD and 0.1%, priced from 1.7M gas when that
costs more, a 2 tUSD minimum, authorizations good for up to a day, eight on their way at once), and
the gateway an allowance for deposits of one a second, bursts of ten. The relayer sends deposits
from an account of its own, `0xedc4c88a3f6ae8130a2c18babbabd8100cc1a446`, which the terms name and
wallets pin. A first deposit through the gateway shielded in 14 seconds and cleared Railgun's
screening within five minutes. App builds that pin the old token can no longer swap on testnet.

Rollback files are in `/var/backups/zecswap/20261018-deposits-120106d` (root-only):
`maker-config.toml`, `relayer-config.toml`, `nginx-site` (its path in `nginx-site-path`),
`previous-current` and `maker.sqlite`. They predate the windows contract, so undo its switch first.
Rolling back then moves the inventory back on chain:
1. Stop both services.
2. Run `withdraw-inventory all` under the current maker config.
3. Restore both configs and the nginx site, and reload nginx.
4. Run `add-inventory 956670142` under the restored config: the maker's account holds that old tUSD.
5. Point `/opt/zecswap/current` at the release `previous-current` names, and start both.

Deposits then stop. Their journal stays in `/var/lib/zecswap-relayer/deposits.sqlite`.

## October 19 mainnet deposits

At 03:42 UTC `deploy/mainnet/deploy-deposits.sh` (`63803a6`) deployed the Deposits contract on
Ethereum from the relayer's own account, `0xcBcb…017F`:
[`0xebc7508e20bd4467491ee7e7589d5c7d4c5e8cb6`](https://etherscan.io/address/0xebc7508e20bd4467491ee7e7589d5c7d4c5e8cb6),
in block 26,224,004. It takes USDC into Railgun's proxy and screens depositors against Chainalysis'
oracle `0x40C5…C8fb`. Its source and its vault logic (`0x8123…3E39`) are verified on Etherscan.

At 03:48 `deploy/mainnet/enable-deposits.sh` waited for no live swap and gave the relayer
`[deposits]` (`deploy/mainnet/relayer-deposits.toml`: a fee of $0.10 and 0.1%, priced from 950k gas
when that costs more, authorizations good for up to a day) and a build of `63803a6` in
`/opt/zecswap-mainnet/releases/20261019-deposits-63803a6`, now `/opt/zecswap-mainnet/current`. Only
the relayer restarted. Deposits go one at a time through Flashbots Protect,
`https://rpc.flashbots.net/fast?blockRange=10`, never the public mempool ([deposits.md](deposits.md)),
from `0x6ae40b34c32d3feaa0eef53d7e4724160b7a2135`. The mainnet gateway's site, which
`deploy/mainnet/nginx-site.sed` makes from `deploy/nginx.conf`, gained the deposits' allowance.

A first deposit of 1 USDC landed through Flashbots 11 seconds after the relayer sent it, unseen by
the public mempool, and its note was Spendable by 06:54 UTC. `balance.cjs` checks that with
`RAILGUN_NETWORK=mainnet`, and needs the owner's Infura key for its 10,000-block log queries. The
test wallet's seed is in `.testnet/deposit-test-wallet` on the owner's machine. The minimum stays at
1 USDC, the owner's call; `deploy/mainnet/relayer-deposits.toml` stages 5, which means `min_value =
5000000` in the relayer's config and a restart of the relayer.

Rollback files are in `/var/backups/zecswap-mainnet/20261019-deposits-63803a6` (root-only):
`relayer-config.toml`, `nginx-site` and `previous-current`. Restore the config and the site, reload
nginx, point `/opt/zecswap-mainnet/current` at the release `previous-current` names, and restart the
relayer. Deposits then stop; their journal stays in
`/var/lib/zecswap-mainnet-relayer/deposits.sqlite`.

## Earlier changes

Each earlier release's checksums and checks are in its manifest, and its rollback files in
`/var/backups/zecswap/<release>` or `/var/backups/zecswap-mainnet/<release>`. What they brought is
described where it now lives.

| Date | Change | Details |
| --- | --- | --- |
| Oct 2 | Testnet contract `0xa067d2…` with single-use, expiring rescue authorizations, replacing `0xbd9a37…`; sponsored reverse funding (`POST /v1/reverse/fund`) | [retired/sepolia-reverse-a067d2.json](../deployments/retired/sepolia-reverse-a067d2.json), [retired/sepolia-relayer-funding.json](../deployments/retired/sepolia-relayer-funding.json), [reverse-flow.md](reverse-flow.md#sponsored-initial-funding) |
| Oct 6 | The maker on crates that schedule NU7, which testnet activated at block 4,465,026, after lightwalletd refused its transactions; the funding fee | [retired/sepolia-nu7-maker.json](../deployments/retired/sepolia-nu7-maker.json), [retired/sepolia-relayer-funding-fee.json](../deployments/retired/sepolia-relayer-funding-fee.json) |
| Oct 7 | The terms-hash contract `0xD75Efc6a…` (#8); Privacy Pass tokens and the issuer | [retired/sepolia-terms-hash.json](../deployments/retired/sepolia-terms-hash.json), [sepolia-tokens.json](../deployments/sepolia-tokens.json), [tokens.md](tokens.md) |
| Oct 8 | The issuer's `android-key` attestation; per-kind gateway allowances; the monitors; private Railgun sends | [sepolia-attestation.json](../deployments/sepolia-attestation.json), [sepolia-monitoring.json](../deployments/sepolia-monitoring.json), [sepolia-railgun-sends.json](../deployments/sepolia-railgun-sends.json), [sepolia-railgun-spent.json](../deployments/sepolia-railgun-spent.json), [Token issuer attestation](#token-issuer-attestation), [Operating](#operating-the-deployment) |
| Oct 9 | Market prices from providers, the economics exports, and every swap and relayer fee paying its gas | [sepolia-profitability.json](../deployments/sepolia-profitability.json), [sepolia-swap-fees.json](../deployments/sepolia-swap-fees.json), [Each swap pays its own network cost](#each-swap-pays-its-own-network-cost) |
| Oct 12 | Tickets and `network` names; a sending account per kind of send, its float at the fee cap; the relayer's Telegram messages; sends at the node's tip; gas alerts on sends; a swap's relayer requests in turns, each step sent once; one read a pass | [sepolia-tickets.json](../deployments/sepolia-tickets.json), [sepolia-lane-float.json](../deployments/sepolia-lane-float.json), [sepolia-relayer-alerts.json](../deployments/sepolia-relayer-alerts.json), [sepolia-node-tip.json](../deployments/sepolia-node-tip.json), [sepolia-gas-on-sends.json](../deployments/sepolia-gas-on-sends.json), [sepolia-concurrent-swaps.json](../deployments/sepolia-concurrent-swaps.json), [sepolia-rpc-budget.json](../deployments/sepolia-rpc-budget.json), [RPC use](#rpc-use) |
| Oct 14 | 1,000 tUSD of inventory; the maker's monitor reads every account's funds from one wallet summary; testnet accepts take no tokens | [sepolia-monitor-funds.json](../deployments/sepolia-monitor-funds.json), [Testnet as it runs](#testnet-as-it-runs) |
| Oct 17 | Sweeps carry no outgoing copy a swap's user could read, on both networks | [sepolia-sweep-outgoing-copy.json](../deployments/sepolia-sweep-outgoing-copy.json), [mainnet-beta.json](../deployments/mainnet-beta.json) |
| Oct 18 | Mainnet's maker on testnet's maker build | [mainnet-beta.json](../deployments/mainnet-beta.json) |
| Oct 19 | Testnet's swap cap from 20 to 200 tUSD (`/var/backups/zecswap/20261019-swap-cap-200` holds the config before) | [Testnet as it runs](#testnet-as-it-runs) |

## Deployment sequence

`deploy/testnet/deploy-windows.sh` and `deploy/testnet/switch-windows.sh` are the latest worked
example of these steps.

1. Deploy the contract with the network's `RAILGUN` proxy and its windows. Sepolia prices new state
   far above what `forge script` simulates, so a script's own gas limit runs out: send it with
   `cast send --gas-limit <the node's estimate plus a margin> --create <bytecode ‖ constructor
   arguments>`, as the deploy scripts do (27.5M gas on Sepolia, where it took 4.2M before the
   repricing). The anvil fork the live suite runs on keeps the old prices, so check gas limits
   against Sepolia itself: a sponsored reverse funding uses about 2.7M of the relayer's 4M. Record
   the chain ID, contract, token, transaction and block in a manifest under `deployments/`.
2. Configure the maker and an independent relayer for that exact deployment. A new contract gets a
   fresh maker store: a store from an older maker lacks columns the current one needs, and the
   maker refuses to start on it, so let an old store's swaps settle under the build that made them.
   A fresh store numbers maker shares from the clock, above any an earlier store handed out, so it
   may keep the same `MAKER_ROOT_SECRET`. The maker refuses to start if its secret or key is not the
   one its live swaps opened under, or if its `[timing]` or `[reverse]` doesn't fit the contract's
   windows. Enable `[reverse]` from the example config, with `MAKER_ZCASH_SEED` in the environment.
   The relayer serves one token and one maker: set its `token` and `maker`. For private Railgun
   sends, give it `[railgun_sends]` and its own Railgun wallet's seed in `RELAYER_RAILGUN_SEED`
   ([railgun-sends.md](railgun-sends.md)), with its journal in its state directory.
3. Run `zecswap-maker --config <maker-config> zec-inventory` for the seed-derived ZEC inventory
   address and balance, and fund it and the services' EVM accounts. To move the inventory from a
   retired contract, run `withdraw-inventory all` under the old config and `add-inventory <amount>`
   under the new one. Write the token issuer's key and the maker's return key, under which it hands
   tokens back, each with `zecswap-issuer keygen <file>`; the return key's public half goes into the
   app build, which pins it. To take tokens, give the maker a `[tokens]` table with the issuer's
   public half and the return key's file ([tokens.md](tokens.md)); a `[tokens]` without
   `return_key` doesn't load. Give the issuer an `[attestation.android-key]` table
   ([Token issuer attestation](#token-issuer-attestation)); one on `insecure-test` starts only with
   `allow_insecure = true`. Run `zecswap-issuer serve`, `zecswap-maker serve` and `zecswap-relayer`,
   each with `--config`, under systemd (`deploy/systemd/`).
4. Give the host a [Workers VPC Service](https://developers.cloudflare.com/workers-vpc/get-started/)
   over a remotely managed tunnel. `deploy/nginx.conf` routes `/maker/`, `/relayer/` and `/issuer/`
   to the loopback listeners, and `deploy/worker/` is the stable `workers.dev` front; a custom domain
   is optional. Keep tunnel credentials and service secrets out of version control.
5. Run the checks under [Public verification](#public-verification) before an app pins the
   deployment.

## Switching to mainnet

Mainnet beta still runs the lock contract, with a release from before the windows port. Builds of
main refuse the lock contract, so moving mainnet takes a windows contract of its own
(`scripts/deploy-contract.sh <name> <min window> <late window> <wallet>`; the staged maker in
`deploy/mainnet/maker.toml` assumes 1 hour and 24 hours), with the lock contract's swaps settled
first, as testnet's switch did.

One setting picks the network: `network = "testnet"` or `"mainnet"` in the maker's, the relayer's
and the issuer's configs. `crates/zecswap-api/src/network.rs` holds what each network fixes, and
each service refuses to start on anything of another network:

- The maker and the relayer refuse an EVM RPC on another network's chain (mainnet: Ethereum, chain
  1; testnet: Sepolia, Base Sepolia or a local anvil), a contract paying into a Railgun other than
  the network's own there (mainnet `0xFA7093CD…`, Sepolia `0xeCFCf3b4…`), and on mainnet a token
  other than USDC (`0xA0b86991…`).
- The maker refuses a `sweep_to` of another Zcash network, and its wallets a lightwalletd serving
  another (each asks its server once). `lightwalletd` defaults to the network's.
- The relayer's fundings go through Railgun's own Relay Adapt on its chain, the default for
  `[reverse_funding] relay_adapt`; another one is refused.
- The issuer on mainnet refuses `insecure-test`, whatever `allow_insecure` says.
- `add-inventory --mint` is refused on mainnet, and the CLI's `--network` picks its lightwalletd.
- The relayer and the maker on mainnet refuse to start without `[reveals] private_rpc`, below.

Claims and reverse refunds reveal a share in their calldata. One that landed after its window shut
would revert with the share public, and the other side, whose window opens next, could take both
halves of the swap. So on mainnet the relayer sends them through Flashbots Protect, never the public
mempool: `[reveals] private_rpc`, staged in `deploy/mainnet/relayer.toml` as
`https://rpc.flashbots.net?hint=hash&builder=Titan&builder=Quasar&builder=Eureka&blockRange=10`.
Protect lands a transaction only if it succeeds, and drops one it couldn't land within its
`blockRange`. No node shows one until it mines, so the relayer sends them one at a time from its
claims account, holding the account's turn until each mines or 5 blocks past its range (15 here) go
by, and answers a dropped one `503`, to post again; a restarted relayer sends none for those 15
blocks. Only the broadcasts go to Protect, which limits reads to a few hundred an IP every five
minutes: reads, nonces, simulations and receipts stay on `evm_rpc`.

The relayer refuses Protect's `/fast`, which shows TEE searchers whole transactions, failed ones
too; any `hint` but `hash` alone, since the default hints share logs and `Claimed` logs the share;
and any parameter but `hint`, `blockRange`, `builder`, `refund`, `originId` and `auctionTimeout`.
Flashbots' own builder, BuilderNet, always gets the transaction, but built about 14% of blocks on
October 19: alone it would miss a 10-block range one time in five. Naming Titan, Quasar and Eureka,
as Flashbots' builder registry spells them, covers about 95%; each sees the whole transaction, as it
must to include it. Confirm the names on the first mainnet claim, on Protect's status page for the
transaction; names no builder answers to leave it to BuilderNet alone, slower but private. Testnet
keeps the public mempool, since Flashbots' Sepolia relay has included nothing since block
11,856,337.

Wherever a reveal goes, it is broadcast only while its window still has `claim_margin` left once
its turn comes, judged on the newest block by the later of its time and the wall clock; one whose
window ran under the margin while it waited is refused with `400`, nothing sent, as on arrival. A
reveal sent privately is never simulated, as a simulation would show the RPC its share: it carries a
fixed gas limit. A send through the public mempool, a testnet reveal among them, goes again at the
same nonce at the fees then when three blocks pass without it. The maker's own cancels, refunds and
reverse claims reveal its share too, so on mainnet they go through Protect as well, with the
relayer's endpoint and builders: `[reveals] private_rpc` in `deploy/mainnet/maker.toml`. The maker
checks it as the relayer does and refuses to start on mainnet without it; its other contract calls
then go through Protect with them, one at a time.

Swaps use the Ironwood pool, which NU6.3 brought (mainnet height 3,428,143), so a maker on the
pinned crates runs on mainnet. They schedule NU7 on testnet alone: before NU7 activates on mainnet,
the maker and the app must move to crates that schedule it there, or their Zcash transactions are
refused, as testnet's were from its NU7 on October 4 until the maker moved.

A mainnet deployment also needs its own: a contract with `RAILGUN` set to the mainnet proxy; the
maker's and relayer's `contract`, `token`, `maker` and RPC URLs ([deploy/mainnet/](../deploy/mainnet/)
holds the beta's configs); mainnet ETH and USDC; secrets of its own (`scripts/mainnet-secrets.sh`,
whose BIP-39 phrases restore the maker's ZEC inventory in any Zcash wallet and the relayer's fees in
any Railgun wallet); its services, which `deploy/mainnet/install.sh` puts beside the testnet's on the
droplet under names, users, folders and ports of their own (`zecswap-mainnet-*`, `127.0.0.1:8797` to
`8799` behind nginx on `8090`); the issuer's production packages and signing digests; a Worker of
its own (`deploy/worker/wrangler.mainnet.jsonc`) on a VPC service of its own through the droplet's
tunnel; mainnet observability labels ([observability.md](observability.md)); and the dashboard's
mainnet environment. Mainnet counts deposits after 2 confirmations, the owner's choice for the
beta's small swaps (the wallets' default is 10). Funding sponsorship has no aggregate budget, which
matters more on mainnet.

Funds leave a mainnet deployment by these routes alone. `max_awaiting_deposit = 0` and a restart
refuse every new swap while the watchtower settles the open ones; `zecswap-maker withdraw-inventory
all --to <address>` then moves the USDC no swap holds; the maker's Zcash phrase restores its ZEC
inventory in a Zcash wallet, with the maker stopped; forward swaps' ZEC is swept to `sweep_to` as
they settle; and `zecswap-relayer drain-lanes --to <address>`, with the relayer stopped, empties its
sending accounts. The maker's and relayer's own ETH leave with their keys.

## Operating the deployment

Grafana Cloud telemetry, resource limits, credential handling and its rollback are in
[Bridge observability](observability.md).

`scripts/build-server.sh` cross-compiles the Linux release binaries from macOS with the pinned Rust
toolchain, cargo-zigbuild and Zig; it needs `uv` and `rustup`. Upload the binaries from
`target/x86_64-unknown-linux-gnu/release/` to a new `/opt/zecswap/releases/<release>/`, mode `0755`
(systemd can't run them from `0700`), then switch `/opt/zecswap/current` and restart the services.
Keep the previous release and its `previous-current` for rollback, and never replace or roll back
wallet databases with a binary rollback.

The systemd units are in `deploy/systemd/`: `zecswap-maker`, `zecswap-relayer`, `zecswap-issuer`
and `zecswap-tunnel`, all enabled at boot, with `zecswap-issuer-status.timer`. The maker uses one
proving thread and two async workers, so proving doesn't hold the only API worker. The tunnel's
credential is a systemd credential loaded from `/etc/zecswap-tunnel/token`. API listeners are
loopback-only, and the firewall lets in SSH alone. The host has 1 GB of RAM, a 25 GB disk and a 1 GB
swap file.

A config must be readable by its service account: root ownership, the service's group
(`zecswap-maker`, `zecswap-relayer`) and mode `0640`. Keep that group and mode when replacing a
file, as `cp -a` does. Environment files stay root-owned `0600`, since systemd reads them before it
switches to the service account.

From `deploy/worker/`, run `npm ci`, `npm run types`, `npm run check`, `npm test`, then
`npx wrangler deploy --dry-run` before `npm run deploy`. The VPC binding is pinned in
`wrangler.jsonc`, a template whose real ids sit in the gitignored `wrangler.local.jsonc` that
`npm run deploy` uses, and the Worker needs the maker's `MAKER_TICKET_SECRET` as its own secret
(`npx wrangler secret put TICKET_SECRET`). The gateway streams request bodies unchanged, disables
caching and never retries financial requests. It forwards only the `Authorization`, `Content-Type`
and `Content-Length` headers, so it passes on nothing else a client sends, nor the client address
Cloudflare attaches.

Each accept answers with a ticket for its swap: the swap id and an HMAC-SHA256 of it under
`MAKER_TICKET_SECRET`, a secret of its own that leads to none of the maker's keys. Wallets send it
in the `ZecSwap-Ticket` header of every relayer request for the swap. The Worker checks it with
WebCrypto, never asking the origin, and only then adds a `ZecSwap-Swap` header naming the swap; it
drops both headers from what a client sends.

Every request reaches nginx from `cloudflared` on loopback, so nginx can't tell callers apart, and
limits are never per IP. `deploy/nginx.conf` gives each kind of request its own allowance instead,
so a flood of one kind never refuses another:

| Requests | Allowance |
| --- | --- |
| A swap's relayer requests (fundings, readies, claims and held claims, payouts, refunds, rescues) with a ticket that checks out | 1 a second for each swap; up to 10 more wait their turn |
| The same requests without one | 2 a second shared; up to 20 more wait their turn |
| Quotes | 2 a second, bursts of 30 |
| Accepts | 10 at a time; 2 a second, bursts of 20 |
| Private Railgun sends and withdrawals: `railgun/transact` | 2 a second, bursts of 20 |
| Deposits | 1 a second, bursts of 10 |
| The issuer's challenges and tokens | 2 a second, bursts of 20 |
| Reverse swap status, which reads the chain on every request | 2 a second, bursts of 20 |
| The three monitors | 1 a second, bursts of 10 |
| Everything else | 10 a second, bursts of 50 |

Each swap's status also has its own allowance, of one a second with bursts of ten, so an app polling
its swap too fast slows only that swap. Relayer requests without a ticket share their small
allowance while apps that send none are about; once every app sends tickets, setting
`$zecswap_tickets_required` to 1 in `deploy/nginx.conf` refuses them with `403` instead. Past an
allowance nginx answers `503` with `Retry-After` and the gateway's `unavailable` error, which clients
take as a passing failure; the services' own `503`s pass through unchanged. It never answers `429`,
which from the issuer means a device's tokens for the day are spent. `zecswap-cli` waits such a
`503` out and sends the request again, for 90 seconds in all.

A quote reserves nothing: it costs the maker a row and at most two chain reads, its gas price and
inventory readings lasting a minute and half a minute. An accept spends the maker's gas on its
`open` and answers once the open lands, and the maker's opens queue one a block, so ten accepts at a
time finish within the three minutes a client waits for one; past them, an accept is turned away at
the gateway before it costs anything, its quote still good. An accept paid with a token is posted
twice, the first answered with the token's challenge.

A forward accept spends maker gas before the user's ZEC deposit, and a reverse accept reserves the
maker's ZEC until its funding deadline. `max_awaiting_deposit` caps how many swaps wait on their
users at once, in both directions. With `[tokens]`, every accept spends a Privacy Pass token, handed
back once the user pays in, so a device walks away from at most `tokens_per_day` swaps a day
([tokens.md](tokens.md)); that binds because the issuer checks Android key attestation. The relayer
serves only its configured token and maker, on every route; the funding route keeps its transaction
checks and gas caps, and there is no aggregate sponsorship budget. Tickets are only as scarce as
accepts, so rely on the cap and the tokens, not on the allowances. Keep the forward ordering:
depositing ZEC before the escrow exists would remove its contract-backed recovery.

Keep the maker seed, root secret, `maker.sqlite`, `wallet.sqlite` and `flow-wallet.sqlite` together
in protected backups: the VPS disk is persistent, not an off-host backup. Keep both kinds of database
across upgrades and restarts: settlement keeps accounts, birthdays, viewing keys, transaction history
and sweep IDs, and revisits completed swaps for reorg recovery. The maker records its intent to
cancel a swap and that it sent a swap's ready before either send, so a swap it began to cancel is
never made ready, and one it made ready never cancelled, whatever a restart or a reorganisation
shows. Restoring an old snapshot also needs newer on-chain swaps reconciled before new quotes.

Forward settlement waits `evm_confirmations` (top level, default 12) and reverse settlement
`reverse.evm_confirmations`. Zcash sweeps wait the deposit depth (`confirmations`, default 10),
counted from the fully scanned height. Lightwalletd responses have a 30-second idle and a 120-second
total deadline, streams included. Rebroadcasting a recorded ZEC transaction can return a backend
duplicate error while it is in the mempool or mined; the adapter then checks `GetTransaction` for the
same ID and exact bytes before taking it as sent, and keeps the original error otherwise.

### RPC use

The maker and the relayer read Ethereum through the RPC URL in their configs, and Alchemy caps what
an account spends a month (30M compute units on its free tier) whichever of its keys spends it: give
a deployment an account of its own, apart from development's and the apps', or a pay-as-you-go plan.
Each service logs its RPC when it starts, by host and a fingerprint of its key (the first eight hex
digits of its SHA-256, never the key), and every hour its requests by method with the compute units
Alchemy bills for them (`RPC requests in the last period`); each monitor exports the same since its
start as `rpc`. The hosted testnet's services use the key with fingerprint `f6773aad`.

| What | Reads | Compute units |
| --- | --- | --- |
| Maker watchtower, every 15 s | With no live swap, one `eth_chainId`, billed nothing. Otherwise the latest block, every live swap in one call, and those settling a few blocks deep in another. A swap whose claim or refund a finalized block holds is never read again | 0 idle; 50 to 75 a pass, however many swaps |
| Maker Zcash worker | The watchtower's reads; reverse swaps read the same way | 0 without reverse swaps |
| Transaction observer | Each block's events once, in ten-block windows, every 15 s while swaps are live and every 90 s otherwise; the newest twelve again every 2 minutes, or 10 | about 85k a day idle |
| Quotes and reverse status | The gas price once a minute, the inventory every 30 s; status from the passes' reads | little, however often asked |
| Relayer lanes | Every lane's balance in one call, after each of its sends and every 2 minutes | about 35k a day |
| Relayer fees | The gas price, once a minute at most, however often terms are read | at most 30k a day |
| Relayer held claims, every 4 s | With none held, nothing. Otherwise the latest block, and on each new one every claim due in one call, with the swap a ready's depth back for one waiting on its ready. One that can't go before a known time, as in the maker's late window, is looked at every 5 minutes | 0 idle; about 75 a block while one is held |

Idle, the two spend about 0.15M compute units a day, and a swap about 20k more beside its own
transactions.

### Held claims

With `[held_claims]`, whose `journal` is a SQLite file of its own, the relayer takes
`POST /v1/claim/hold`: a forward claim, the body `/v1/claim` takes, posted once the deposit is out,
so its user may leave the app (`zecswap_api::relayer::Held` has the answers). It checks the claim
as `/v1/claim` does, all but the window, keeps it, and sends it through the claims lane the moment
its window opens: on a ready 2 blocks deep, from `t0` where the maker never readies, and from
`t1 + LATE_WINDOW` where the maker didn't refund in its late window. The payout keeps the fee the
claim was held with, though that fee's quote expires meanwhile. A held claim and its share are
deleted, the journal overwriting what it deletes, once its swap is paid out or refunded. Without
`[held_claims]` the route answers `400`. The testnet relayer holds claims since October 22
(`20261021-held-claims-47650c6`, [October 20](#october-20-windows-contract)); no wallet has handed
one over yet.

The relayer then holds each user's share long before the claim reveals it. It can't spend the
deposit with it, as the joint account's `nk` and `rivk` go only to the maker, but handed to the
maker it would let the maker refund and sweep the deposit too: the owner accepts that trust while
running both. No log, error, metric or alert carries a share, nor any request before its claim's
own send. The logs name each held claim's swap and step: `held`, `replaced` (a new payout),
`sent`, `deleted` (paid out), `dropped` (refunded), and `failed` with its kind. Besides
`heldClaims`, the monitor's operations count the hold requests (`hold_claim`) and the sends of held
claims (`held_claim`).

### Token issuer attestation

With `[attestation.android-key]`, the issuer gives tokens only to an install of the app whose key
the phone's secure hardware made and certifies up to Google's root, on a phone with a locked
bootloader that booted verified, and it counts each day's tokens by that key. Scripts, emulators,
phones with unlocked bootloaders or self-signed boot (GrapheneOS, CalyxOS), and other apps get none,
and an install gets no more by asking more often. It does not stop someone with a genuine phone from
reinstalling the app, or clearing its data, for a new key and a fresh allowance; closing that needs a
check tied to the phone, such as Play Integrity's device recall, or a bond. Nor does it hold against
someone who breaks into a locked phone's running system.

The issuer needs, readable by its service account: Google's attestation roots as PEM files
(`roots`), saved from `https://android.googleapis.com/attestation/root`; Google's status list
(`status_list`), saved from `https://android.googleapis.com/attestation/status` and refreshed at
least daily by a timer that writes a new file and renames it into place (the issuer rereads it when
it changes, and refuses to start if it can't read it); and the app's package and the SHA-256 of its
signing certificate. The issuer's `name` is in every install's key: changing it makes every install
start over with a new key. `deploy/systemd/zecswap-issuer-status.timer` refreshes the list every six
hours, and its service puts a new list in place only once it reads.

The hosted testnet issuer runs `android-key` as `zecswap-testnet-issuer`, three tokens a day,
counted by each install's attested key. It takes `xyz.justzappit.zapp.testnet` and its `.debug`,
`.foss.debug` and `.internal.debug` builds signed with the development debug key, the key held at
least in the phone's trusted environment; a release-signed build needs its certificate's digest
added to `signing_digests`. Google's roots are in `/etc/zecswap-issuer/roots/`, and the timer keeps
the status list in `/var/lib/zecswap-issuer/attestation-status.json`. `insecure-test` believes any
caller, and the issuer starts in it only with `allow_insecure = true`.

Run the issuer apart from the maker, ideally by another party: it sees each install's key on every
fetch, and the maker must never see an attestation. It logs no chain, key, challenge or device id,
and keeps only today's counts.

## Public verification

After a deploy, check through the public gateway:

- Maker `GET /v1/info`: the exact expected chain ID, contract, `contractKind`, windows, token, Zcash
  network and `reverseEnabled: true`.
- Maker `GET /healthz`: 204 after both an EVM pass and a successful Zcash sync. Either one older than
  three tick intervals returns 503 and closes quote and accept admission; EVM deadline processing
  goes on while Zcash is stalled or proving.
- Relayer `GET /v1/terms`: the same chain ID, contract and windows, with an independent relayer
  address.
- A forward and a reverse quote: valid typed terms and enough funded inventory. Check every deadline
  and the maker proof before accepting.
- A swap through the gateway, as `zecswap-cli swap --relayer` runs one.

Keep API caching disabled. A returned transaction hash means submitted, not confirmed: after a
timeout, reconcile chain state before retrying any financial action. The app must verify the escrow
and the ZEC it receives itself, as [reverse-flow.md](reverse-flow.md) describes.

### Telegram bridge alerts

Set `TELEGRAM_BOT_TOKEN` and a numeric `TELEGRAM_CHAT_ID` in the maker's protected environment,
both or neither; without them alerts are off. The hosted testnet reuses the onramp gas monitor's
bot and private chat, and only calls Telegram's `sendMessage`: it never changes the bot's webhook or
consumes its updates. Keep the pair out of Git, dashboard variables, browser responses and command
arguments. On the host it is in the root-only `0600` `/etc/zecswap-maker/telegram.env`, loaded by a
drop-in, `/etc/systemd/system/zecswap-maker.service.d/telegram.conf`:

```ini
[Service]
EnvironmentFile=/etc/zecswap-maker/telegram.env
```

Reload systemd and restart the maker after changing it. Under the service's environment and
account, `zecswap-maker --config /etc/zecswap-maker/config.toml telegram-test` sends one labelled
setup message without a swap; success means Telegram took it, not that anyone read it.

Each swap sends two messages, queued in the same SQLite transaction as the state change they report:
one when its quote is accepted and one when it ends. Each names the network, the direction and the
exact amounts, gives the time and the first eight hex digits of the swap ID (the dashboard's search
finds a swap by them), and ends with one link. The last says how the swap ended and how long it took:
completed, refunded, cancelled (no ZEC arrived) or expired (its escrow never opened or was never
funded, or the maker sent no ZEC in time), with a line on why unless it completed, and links the
claim, refund or payout that settled it on Etherscan, or the dashboard if the maker hasn't indexed
one yet. A rescue of a payout Railgun sent back sends one more message. Nothing else about a swap
sends one: deposits, confirmations, readies and sweeps are on the dashboard. Messages never carry the
contract, the maker's address or block numbers.

The relayer sends one message for each Railgun send it broadcasts for a wallet, once the send mines,
queued in its journal in the same write as the send's cost: a private send gives the fee it paid, an
unshield the USDC it took out and the fee, and a reverted send that nothing moved and the relayer
paid the gas. None names an address. Give the relayer the maker's pair with a drop-in,
`/etc/systemd/system/zecswap-relayer.service.d/telegram.conf`, naming the same `EnvironmentFile`
(systemd reads it as root), and restart it; its `sending Railgun transactions` log line then shows
`alerts=true`. A relayer without `[railgun_sends]` sends nothing.

Delivery runs apart from the watchtower, with a ten-second request timeout. The durable queue retries
outages with backoff, respects Telegram's rate limits, and keeps event IDs after delivery so restarts
don't replay them; a message can repeat if Telegram takes it but its acknowledgement is lost. Settled
history is never replayed, and live swaps are enrolled when alerts are turned on. An error sends one
message once it has lasted five minutes, and no more until a pass succeeds, so a blip of the RPC or
lightwalletd sends nothing: a swap the watchtower can't advance, failing or panicking passes, and a
failing Zcash sync. `maker.sqlite` backups hold the swap alert text, so protect them.

`[gas_alerts]` watches the ETH of the account its service sends from: the maker's own, or the
relayer's own (`RELAYER_PRIVATE_KEY`'s, which tops up its other sending accounts; it needs
`[railgun_sends]`, whose journal queues the messages). Only an account's own sends spend its ETH, so
it is read at start and each time one of its sends ends, mined or failed. It sends one message when
the account drops under `low_wei`, with its balance and address, and one when it is back at
`recovery_wei`; a first healthy reading sends nothing. A top-up shows in none of its sends, so while
the account is low it is also read every `interval_seconds` (60 to 3600). Both services take the
same section:

```toml
[gas_alerts]
low_wei = 20000000000000000       # 0.02 ETH
recovery_wei = 50000000000000000  # 0.05 ETH
interval_seconds = 300
```

Each monitor's `gasAlerts` names the account it watches, and `GET /v1/monitor` includes
`notifications`: whether alerts are on, how many wait and since when, the last delivery and a
sanitized delivery error, never the bot token or chat ID. The dashboard shows it as the Telegram
alerts check. Mainnet runs the same code with its own configuration and data: give its maker and
relayer the pair (the testnet's chat will do, since every message names its network and links its own
chain's explorer and dashboard), and each a `[gas_alerts]` for its own account, then run
`telegram-test`. Deduplication IDs include the deployment.

### Dashboard monitoring

Live USD pricing is enabled by `[pricing.market]` in the maker configuration:

```toml
[pricing.market]
token_decimals = 6
refresh_seconds = 60
max_age_seconds = 300
providers = ["coinmarketcap", "alchemy"]
```

`providers` is required and ordered: each refresh asks the first, and the next only while it fails,
so quotes keep their price through one provider's outage. Each provider needs its key in the
maker's protected environment: `ZCASH_CMC_KEY` for CoinMarketCap, `ALCHEMY_API_KEY` for Alchemy's
Prices API, sent as a bearer header, never in a URL; neither is ever a dashboard variable or in a
browser response. Quotes and the monitor fetch ZEC and USDC prices together (CoinMarketCap IDs 1437
and 3408; Alchemy by symbol) through a shared 60-second cache and one request at a time, with ETH
(1027) for gas values alone: missing or stale, it never touches quotes. ZEC/USDC is ZEC/USD over
USDC/USD in six-decimal token units, with the spread and zatoshi rounding in both directions. Both
prices must be within `max_age_seconds`: a failing provider's last price serves only that long, and
then new quotes return 503 `unavailable`; failed providers are retried at most every ten seconds.
Issued quotes keep their stored amounts until they expire, and settlement never waits on prices.
Without `[pricing.market]` the maker prices from its fixed price, for local tests only. The monitor's
`pricing.source` names the provider behind the current price, and `pricing.providers` each one's last
error.

`GET /maker/v1/monitor` is a read-only operations export for `zapp-dashboard`,
`GET /maker/v1/monitor/swaps/{id}` the same record for any one swap, and
`GET /maker/v1/monitor/economics?since=<unix seconds>` what each swap accepted since then (30 days
back by default, newest first, at most 1,000) earned and cost. All three are off unless
`MAKER_MONITOR_TOKEN` is set, at least 32 characters, and take `Authorization: Bearer <token>`; the
dashboard holds the same value as `BRIDGE_TESTNET_MONITOR_TOKEN`. None returns a seed, share,
viewing key, user authorization or wallet account identifier.

The export (`schemaVersion` 2) holds only what the maker knows: its shielded ZEC inventory (total,
spendable, reserved, available) and the ZEC in unsettled swaps' deposit accounts; quote and swap
counts and how many swaps wait on a deposit, against `max_awaiting_deposit`; watchtower readiness,
sync recency and errors since restart; pricing and timing policy, the contract's kind and windows;
the gas alert accounts; the token gate's day ([tokens.md](tokens.md)); and up to 50 swaps, live ones
first. Each swap carries its quote ID, when it was accepted, opened and settled, whether it is
archived or cancelling, whether its token went back, its deposit and sweep transaction IDs, wallet
funds, deadlines, and every recorded Ethereum event with its block time. Settled counts include
expired and refunded swaps. Per-swap errors are from the last completed pass, and the counter resets
on restart; the details are in the maker's logs.

The economics export (`schemaVersion` 1) gives each swap its amounts, the market prices its quote
was made at (`quote`), every Ethereum transaction on it and the maker's Zcash sends on it. A
transaction is listed if one of the swap's events was in it or the maker sent it for the swap, a
reverted one included: its sender, success, gas, effective gas price, the escrow token it paid a
relayer, and ETH/USD at its block's time. A Zcash send (`sweep`, reverse `deposit`, or `recovery`
after a refund) has its exact fee and ZEC/USD when it was sent. Prices are the maker's own when it
saw the event within five minutes (`live`), else Alchemy's nearest five-minute candle within a
quarter hour (`history`, with `alchemy` among the providers); a value that is neither stays missing,
and a time Alchemy has no price for is asked about again an hour on. Where the token logs no
`Transfer`s, as testnet's before October 18 didn't, a relayer's fee is read from the escrow's
`PaidOut` and `Rescued` events and a funding's Relay Adapt calldata.

The maker's monitor reads nothing from the chain: contract states and USDC and ETH balances are the
dashboard's to read, on its own RPC, so monitoring never spends the RPC the watchtower needs. It
never syncs the wallet, proves, sends or writes inventory; with the wallet busy it shows the last
inventory reading and sets `walletBusy`.

The relayer's `GET /relayer/v1/monitor` (`RELAYER_MONITOR_TOKEN`, in
`/etc/zecswap-relayer/secrets.env`) counts, for each operation since it started, the transactions
sent, the requests refused, and those that failed by kind (`reverted`, `unconfirmed` or `internal`,
never a message), with the gas and wei they burned, from which the dashboard works out how long its
ETH lasts. With `[held_claims]` it adds `heldClaims`: the claims held now, and those sent and
dropped since it started ([Held claims](#held-claims)). With `[railgun_sends]`,
`GET /relayer/v1/monitor/sends` lists each private send and
withdrawal it paid the gas of, with what it burned and the fee it was paid
([railgun-sends.md](railgun-sends.md)). The issuer's `GET /issuer/v1/monitor`
(`ISSUER_MONITOR_TOKEN`, in `/etc/zecswap-issuer/secrets.env`) shows the day's token totals, its
refusals by reason and its status list's age. Both are closed while their token is unset and take
tokens of at least 32 characters.

The dashboard checks the deployment and the token's precision before formatting USDC: the testnet's
tUSD is pinned at six decimals, and mainnet has no default precision. Mainnet uses its own dashboard
environment variables and monitor tokens. The dashboard pins each network's contract in
`lib/bridge/config.ts`, which Vercel's `BRIDGE_<NETWORK>_CONTRACT` overrides: change both with
every contract redeploy.

### Each swap pays its own network cost

The spread is a share of the swap, but the maker's gas and Zcash fee are the same whatever its size,
so a small swap's spread can't cover them. With `[pricing.costs]` every quote charges them on top of
the spread, at the chain's gas price when it is made (read at most once a minute) and the quote's own
ZEC, USDC and ETH prices:

```toml
[pricing.costs]
forward_gas = 450000      # ZEC to USDC: the maker's open and ready (412k on Sepolia)
reverse_gas = 400000      # USDC to ZEC: its claim (with the lock contract's claim lock, 362k on Sepolia)
zcash_fee_zat = 15000     # the forward sweep, or the reverse deposit (10,000 so far)
margin_bps = 2000         # for gas prices that rise before the sends land
```

A ZEC to USDC quote asks that much more ZEC for the same USDC; a USDC to ZEC quote pays that much
less ZEC, and a swap too small to carry its cost gets no quote. Quotes say what they charged
(`networkCost`, in token base units), and the economics export keeps it with the quote's prices
(`quote.networkCost`, `quote.gasPriceWei`). It needs `[pricing.market]`, which then needs ETH's price
too: quotes pause while ETH's price or the gas price can't be read. Measure the gas on the chain
served, from the dashboard's costs by operation: Sepolia reprices new state, so its budgets overstate
Ethereum's.

The sends pay that gas price. The maker's and the relayer's contract calls tip what the node
suggests, the tip its gas price is made of, at least 0.02 gwei, under a cap of twice the next block's
base fee on top; Railgun sends and fundings pay the gas price itself, or their proof's minimum if
higher.

The relayer's fees follow gas the same way. With `providers` at the top of its config, a payout's
fee is the larger of `fee` and `fee_gas` (the claim and the payout; with the lock contract's claim
lock, 1.65M on Sepolia) at the gas price and ETH price when it is quoted, `fee_margin_bps` on top;
`[reverse_funding]` has its own `fee` and `fee_gas` (the funding and its ready: 2.06M). The terms
quote both, with `feeExpiresAt`: the relayer takes any fee at or above the lowest it quoted in the
hour before, so the fee a wallet shows when a swap starts is the fee its claim pays. Where it can't
price gas, it takes the floor rather than hold a claim up.

### Protocol smoke test

`crates/zecswap-e2e/examples/reverse_deployment.rs` runs a reverse swap against a deployment, funding
the escrow with public test tokens rather than a Railgun unshield. Build it with
`cargo build --profile test -p zecswap-e2e --example reverse_deployment`, so its proving dependencies
are optimized, and set `ETH_SEPOLIA_RPC_URL`, `ZECSWAP_MAKER_URL`, `ZECSWAP_RELAYER_URL` and a fresh
private `REVERSE_TEST_DIR`. `target/debug/examples/reverse_deployment prepare` verifies the pinned
deployment, accepts a one-tUSD quote, imports the deposit account and saves the approval and open
calldata in `funding-calls.json`. Fund those exact calls on Sepolia before the funding deadline,
keeping each signed transaction and its hash before broadcasting. Then set `REVERSE_TEST_DESTINATION`
to a shielded Zcash testnet address and run it with `receive`, which confirms the deposit, authorizes
the ready and sweeps the ZEC home, or with `refund` on another funded test, for the cancellation and
the committed private refund payout. Resume with the same directory after an interruption, and keep
the test seed and wallet: they control the ZEC and the Railgun refund note. The September 28 run, on a
retired contract, is in [retired/sepolia-reverse-bd9a37-smoke.json](../deployments/retired/sepolia-reverse-bd9a37-smoke.json).

### Full bridge flow observation

The monitor exports each visible swap's full Ethereum event history and its `zcashFlow` deposit and
spend observations. A separate `flow-wallet.sqlite` in the maker's data directory imports only joint
accounts' full viewing keys, completed swaps' too, and scans their history; it holds no spending key
and can't sign or broadcast. It runs on a blocking worker apart from settlement, on the configured
lightwalletd, and never refreshes prices. Back it up with `maker.sqlite`; its export holds only
transaction hashes, heights, confirmations and swap amounts. Each pass refreshes up to 500 swaps,
live ones first. Confirmations count from the fully scanned height, leave change out of deposits,
and follow the deposit confirmation policy, and the UI refuses stale observations. A claim or refund
on Ethereum and the final ZEC escrow spend are separate steps: a claim alone doesn't show that the
user's ZEC got home.

Ethereum transactions are indexed from the settlement contract's confirmed events, the app's and
relayer's sends included, by a read-only task that scans ten-block windows without locking the wallet
or fetching prices. Recent blocks go first, and older swaps are backfilled from their earliest
accepted quote. SQLite keeps the cursor and the events' public metadata under the network, chain,
contract and maker. Each block is read once as the cursor passes it, and the newest twelve again
every two minutes while swaps are live (every ten otherwise), to drop events a shallow reorganisation
orphaned; a deeper one needs an operator rescan. Confirmation depth follows
`reverse.evm_confirmations`, or two with reverse swaps off; confirmed does not mean finalized. The
monitor exports `transactions` indexing status and each swap's `evmTransactions`, never log payloads,
revealed shares, RPC credentials or Telegram configuration.

Zcash links go to `https://testnet.zecblock.com/tx/` on testnet and `https://zecblock.com/tx/` on
mainnet. Receipt logs decide Railgun participation: those transactions link to Railscan and
Etherscan, public-only ones to Etherscan.
