# ZecSwap: handoff for the next agent

Non-custodial swaps of shielded ZEC (Ironwood pool) for USDC, both ways. The deposit address's
spend key is split `±(e + z)`: the maker holds `e`, the user holds `z`. An Ethereum contract pays
out against whichever half is revealed, checking it on Pallas, into the user's private Railgun
balance, with no user account on-chain. Wallets also take public USDC at deposit addresses of
their own and shield it into the same balance through the relayer (`docs/deposits.md`). The
contract can still pay an account (the Base route, shelved for the app, still tested). Clients:
the Android wallet `~/dev/zapp/zapp-android`, and Veylor (`~/dev/vizor-fork`), a Vizor fork with
Private USD that another session ports.

Read next (gitignored, on the owner's machine only): `docs/local/plan.md` (decisions, tracks, open
questions); `docs/local/android.md` (the Android handoff; section 2 covers moving to the windows
contract); `docs/local/private-usd.md` (the Railgun route's brief, section 8 what was verified about
Railgun); `docs/local/windows/audit3/` (the third audit: `REPORT.md`, `PORT-SPEC.md`, its model and
vectors). The original design and threat model are in
`~/dev/zapp/ios-zapp/docs/local/shielded-swap-plan.md`; the plan says where it no longer holds.

## Where things stand (2026-10-24)

- **Windows contract.** It replaced the claim and refund locks (PRs #17 and #18, merged
  2026-10-21; "How it behaves" below). All eleven live scenarios pass on a fork of Sepolia on the
  final code (`.testnet/e2e-windows-fork-final.log`). The hosted testnet runs it since 00:01 UTC
  that day: `0x459b0a69e0f736666b66a3ffe0fc17c11f05980d`, 30-minute minimum and 1-hour late windows,
  the maker on a build of `98db2b6` (#22) since 2026-10-22 20:13 and the relayer on `47650c6`'s
  (#20) since 02:44 that day (`docs/deployment.md`, October 20 windows contract;
  `deployments/sepolia-windows.json`). A stress run that day found every limit
  answering as designed (`.testnet/stress-windows-20261021/SUMMARY.md`).
- **Mainnet** runs a windows contract of its own since 2026-10-22 21:21 UTC:
  `0xeB3A00bcB6c9fF626fE57a5dBbDCf01730e7898f`, 1-hour minimum and 24-hour late windows, the maker
  and relayer testnet's own builds with every reveal through Flashbots Protect, held claims on since
  21:50 (`docs/deployment.md`, October 22 mainnet windows contract; `deployments/mainnet-windows.json`).
  The lock contract (`mainnet-beta.json`) is retired, its inventory moved; no swap has run on the
  new one yet.
- **Deposits** are live on testnet since 2026-10-18 (`docs/deployment.md`, October 18 deposits and
  a new test token) and on mainnet since 2026-10-19 through Flashbots Protect (October 19 mainnet
  deposits). Mainnet's minimum stays 1 USDC for now (the owner's call;
  `deploy/mainnet/relayer-deposits.toml` stages 5).
- **Railgun on Sepolia.** A relayed send's change never clears screening there, as Railgun's
  indexer broke its transaction-ID chain (`docs/railgun-sends.md`, Screening; Veylor plans a
  worker that repairs it). Separately, Railgun's Sepolia screening validated no shield that landed
  after 2026-10-20 17:13 UTC, through at least 00:50 the next day: testnet payouts stayed pending.
- **Dashboard** (`~/dev/zapp-dashboard`) reads both contracts. It deploys by hand
  (`npm run deploy`) and pins the testnet contract in Vercel's `BRIDGE_TESTNET_CONTRACT` and in
  `lib/bridge/config.ts`, the testnet token there alone: move them with every redeploy.
- **Next:** the Android client on the windows contract (`docs/local/android.md`, sections 2
  and 10; zapp-android still speaks locks), and Veylor's port. New app code is named
  `AtomicSwap` (zapp-android's `ZecSwap*` types are NEAR's).
- **Open:** the external audit (Track A), the rest of maker hardening (Track B: key custody, store
  backups, runbooks), iOS.
- **Git:** `main` tracks `github.com/JustZappIt/zecSwap` (public, Apache-2.0). Commit and push only when the
  owner asks.

## Layout

| Path | Role |
|---|---|
| `crates/zecswap-core` | Pure crypto: shares, proofs of knowledge, the joint account (UFVK/UA), seed-derived keys (per-swap auth key, deposit addresses in `deposit.rs`), EIP-712 and EIP-3009 signing (`auth.rs`, which keeps the lock contract's digests for swaps still open there), PCZT signing with the combined key |
| `crates/zecswap-railgun` | Railgun keys and `0zk` addresses from the seed, the shield notes payouts go to, fee notes as their receiver reads them; `engine/` checks them against Railgun's engine and wallet SDK (Node: `check.cjs`, `balance.cjs`, `send.cjs`) |
| `crates/zecswap-chain` | Zcash light wallet over lightwalletd (sync, joint accounts, pay, sweep) and the Ethereum client (`evm`: settlement and sends, `judge.rs` for claims, reverse refunds and readies, `deposits.rs`, `railgun.rs`, `funding`) |
| `crates/zecswap-api` | The maker's and relayer's wire types (serde), shared with wallets; the Kotlin port's spec |
| `crates/zecswap-client` | The user side, step by step (open, verify on-chain, deposit, claim or refund), forward and reverse (`reverse.rs`), and a wallet's deposits (`Depositor`): the reference the apps mirror |
| `crates/zecswap-maker` | Maker service: quote API (axum), SQLite store, watchtower; `policy.rs` is the pure forward decision |
| `crates/zecswap-relayer` | Sends what users with no Ethereum account sign for, for one token and maker: claims and refunds (`[reveals]`, through Protect), claims held until their window opens (`[held_claims]`, `held.rs`), payouts, readies, fundings, wallets' Railgun sends (`[railgun_sends]`, `docs/railgun-sends.md`) and deposits (`[deposits]`, `docs/deposits.md`); never run by a maker |
| `crates/zecswap-alerts` | Telegram alerts for the maker and relayer: bot client, a durable queue in each service's SQLite, gas alerts for the account each sends from |
| `crates/zecswap-prices` | ZEC, USDC and ETH prices: `Feed` (providers in order, for quotes and fees) and `History` (Alchemy's five-minute candles, for costs recorded after the fact) |
| `crates/zecswap-tokens`, `crates/zecswap-issuer` | Privacy Pass tokens (RFC 9577/9578) an accept spends, and the issuer that signs each attested Android install a day's tokens (`docs/tokens.md`) |
| `crates/zecswap-jni` | The Android wallet's bindings, `libzecswap.so` (`scripts/build-android.sh`) |
| `crates/zecswap-cli` | Testnet wallet and user CLI (`init`, `status`, `send`, `send-many`, `swap`, `swap --relayer`, `deposit`) |
| `crates/zecswap-e2e` | Live suite: `src/env.rs` (setup, in-process makers, relayer, issuer), `src/scenarios.rs`, `src/sends.rs`, `src/deposits.rs` |
| `contracts` | Foundry: `src/ZecSwap.sol` (the windows contract), `test/deployed/ZecSwapLocks.sol` (the lock contract, for migration tests), `src/ShieldVault.sol` (per-swap payout vaults), `src/Deposits.sol`, `src/Pallas.sol`, `src/Token.sol`; tests incl. `test/fork` and Rust-made vectors in `test/vectors`; `test/utils/TestToken.sol` is testnet's tUSD |
| `deploy/` | The droplet: `nginx.conf` (allowances), `systemd/`, `worker/` (the Cloudflare gateway), `observability/`, `mainnet/` (configs and installers), `testnet/` (the deposits and windows deployments and the droplet's switches to them) |
| `scripts/` | `e2e-testnet.sh` (the live suite), `build-server.sh` (Linux binaries), `deploy-contract.sh` (a mainnet contract), `mainnet-secrets.sh` |

## Commands

```sh
cargo test --workspace                                  # offline tests, plus EVM tests on local anvils (needs `forge build`; the live suite skips itself)
cargo clippy --workspace --all-targets; cargo fmt --all
(cd contracts && forge test)                            # FOUNDRY_DISABLE_NIGHTLY_WARNING=1 hides the nightly banner
(cd contracts && ETH_RPC_URL=… forge test --match-path 'test/fork/*' --isolate -vv)   # real Railgun, USDC and sanctions oracle
cargo build --release -p zecswap-core --example evm_vectors && (cd contracts && FOUNDRY_PROFILE=ffi forge test)
scripts/e2e-testnet.sh [scenario ...]                   # Base Sepolia
scripts/e2e-testnet.sh --railgun [--fork] [scenario ...] # Ethereum Sepolia with Railgun; --fork on a local anvil fork of it
cargo run -p zecswap-cli -- --data-dir .testnet/user status   # the funded test wallet
cargo run -p zecswap-client --example vectors           # known answers for the Android port
# Railgun's engine and wallet SDK (cd crates/zecswap-railgun/engine && npm ci):
node vectors.cjs > ../tests/engine-vectors.json         # regenerate the Rust tests' known answers
node deposit-vectors.cjs > ../../zecswap-core/tests/deposit-vectors.json   # deposit addresses as ethers derives them
cargo run -p zecswap-railgun --example note | node crates/zecswap-railgun/engine/check.cjs
ETH_SEPOLIA_RPC_URL=… node balance.cjs <seed-file> [from-block]   # a wallet's Railgun balance by screening status
RAILGUN_NETWORK=mainnet ETH_RPC_URL=… node balance.cjs <seed-file> [from-block]   # the same on Ethereum
```

The live suite counts deposits after 2 confirmations (`ZECSWAP_E2E_CONFIRMATIONS`), and a full
`--railgun --fork` run takes about 30 minutes. Start it in the background and read the log: the
owner does not want turns spent waiting on it. Never edit `scripts/e2e-testnet.sh` while it runs:
bash reads it as it goes.

## Testnet resources (never print or commit secrets)

- `.env.testnet` (0600): `MAKER_PRIVATE_KEY`, the funder account
  `0x09eD1F966745Be18C711C346242c0974DAd7c3e5` (Sepolia and Base Sepolia ETH; it deploys the testnet
  contracts); `ETH_SEPOLIA_RPC_URL` and `BASE_SEPOLIA_RPC_URL`, the owner's Alchemy URLs (one key,
  which serves `eth-mainnet` too); `INFURA_SEPOLIA_RPC_URL` and `INFURA_MAINNET_RPC_URL`, for
  Railgun SDK log scans; `MAKER_ROOT_SECRET`; the relayer's Sepolia Railgun seed and mnemonic. The
  owner tops up ETH and TAZ when asked.
- Railgun's proxy on Sepolia is `0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea`, on mainnet
  `0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9`. The test wallet's Railgun wallet (seed index 0) is
  `0zk1qyjqfvr4…`.
- `.testnet/user`: the funded Zcash testnet wallet (seed and sqlite; `status` prints its address).
  `.testnet/deposit-test-wallet`: the deposits' test wallet. Each live run keeps its makers' data
  and root secrets in `target/zecswap-e2e/<timestamp>/`.
- The hosted services and the droplet: `docs/deployment.md`, Operating the deployment.

## How it behaves (all tested)

- **Windows.** At every moment at most one side may reveal, by the clock and the stage
  (`docs/reverse-flow.md`, Terms and the contract). Forward: the user claims once ready, or from
  `t0`, until `t1`; the maker cancels an unready swap until `t0` and refunds from `t1` for
  `LATE_WINDOW`, then the user may claim for good. Reverse: the maker claims from ready until `t1`;
  the user refunds while unready and from `t1`. `ready` only before `t0`; `open` and `openReverse`
  refuse a `t0` under `MIN_WINDOW` from landing, or a `t1` under it from `t0`.
- **Reveal judgements** are pure functions on one read, the swap at the newest block's hash
  (`Settlement::read`): `evm/judge.rs` and the maker's `policy::decide`. A window opens by the
  block's time and ends by the later of it and the wall clock, less the margin. Before `t0` a claim
  needs a ready with `READY_DEPTH` (2) blocks above it; a reverse claim one with
  `REVERSE_READY_DEPTH` (4) above it. The audit's 97 cases replay against them
  (`crates/zecswap-chain/tests/windows-spec-vectors.json`, `windows_spec.rs`, `policy::tests`).
- **Contract identity.** Every service and client reads `MIN_WINDOW()` and `LATE_WINDOW()` and
  refuses the lock contract, whose six-word record alloy decodes silently under the four-word ABI.
  The relayer's `/v1/terms` and the maker's `/v1/info` say `contractKind: "windows"`, `minWindow`
  and `lateWindow`, and a forward accept answers `lateWindow`; the client refuses a relayer that
  names others.
- **Saved before sending:** the maker's ready (`ready_sent`, never a cancel behind it) and cancel
  (`cancelling`, never a ready after it); the client's ready signature deadlines and its refund
  request. The relayer holds a reverse refund while its own ready may land and never readies after
  a refund it sent. The client refuses a swap whose `t1` follows `t0` by under twice `CLAIM_MARGIN`.
- **Protect.** The relayer's claims and refunds go through `[reveals] private_rpc`, and the maker's
  cancels, refunds and reverse claims through its own; both are required on mainnet. Loading refuses any URL
  but Protect's default endpoint with `hint=hash` and its few safe parameters
  (`evm::check_keeps_secrets`). A private reveal is never simulated (fixed `REVEAL_GAS`) and holds
  its account until it mines or `blockRange` + 5 blocks pass (`503`, post again); a restarted
  relayer waits that long too (`Steps::note_earlier`). Mainnet deposits share the path
  (`max_pending = 1`).
- **Fees.** On mainnet a reveal's fee cap is 5 gwei higher (`Network::reveal_headroom_wei`), so a
  surge can't strand it past its window; it still pays only the base fee and tip. Sends tip what
  the node suggests, at least `MIN_TIP_WEI`, under twice the next base fee (`evm::fees`); a public
  send unmined three blocks on goes again at its nonce (`land_publicly`). With `providers` every
  relayer fee follows gas (`src/pricing.rs`), and the maker charges its gas and Zcash fee in quotes
  (`[pricing.costs]`).
- **Lanes.** The relayer sends from one account per kind (`lanes.rs`): claims and refunds, readies
  (the lane named `locks`), Railgun sends, fundings, deposits. Its own account sends payouts and
  rescues, keeps each lane at `top_up_transactions` (8) of its largest sends and takes back what's
  over twice that. Every send but a private reveal is simulated before its turn, and wallets'
  calldata must be canonical ABI (`evm/canonical.rs`).
- **Held claims.** With `[held_claims]` the relayer takes a forward claim at `POST /v1/claim/hold`
  once the deposit is out, checked as `/v1/claim` checks one but for its window, keeps it in its
  journal, and sends it as `/v1/claim` would the moment the window opens, at the fee it was held
  with (`held.rs`, `docs/deployment.md`, Held claims). It holds the share long before the claim:
  the owner accepts that trust. Nothing logs or shows a share, and a held claim is deleted once its
  swap is paid out or refunded. Existing routes are unchanged: older clients deny unknown fields.
- **Turns.** A swap's relayer requests take turns, each running to its end in a task of its own
  (`Relayer::in_turn`), and each one-time step goes out once (`send_once`: `503` while one sent
  may land, `200` with nothing sent once landed). Every relayer `503` has `Retry-After`; the client
  posts again until the chain shows the step (`User::relayed`).
- **RPC budget.** A maker pass reads its swaps in one Multicall3 call (`Settlement::swap_states`)
  and never again one a finalized block holds settled (`maker/reads.rs`); idle, a pass is one
  `eth_chainId`. Each connection counts its requests (`evm::RpcMeter`), logged hourly and exported
  as `rpc` by the monitors.
- **Monitoring and alerts.** The maker's `/v1/monitor` (schema 2) reads nothing from the chain;
  `/v1/monitor/swaps/{id}` and `/v1/monitor/economics` add one swap and what each earned and cost.
  The relayer's monitor (`/v1/monitor/sends` too) and the issuer's take their own tokens. Telegram
  gets two messages a swap, rescues, low gas (`[gas_alerts]`) and errors lasting five minutes.
- **Tokens.** With `[tokens]`, each accept spends a Privacy Pass token, handed back once the user
  pays in, so a device walks away from at most `tokens_per_day` swaps a day; the issuer's
  `android-key` mode checks hardware attestation (`docs/tokens.md`). The hosted testnet maker takes
  none since 2026-10-14 (the owner's call; `docs/deployment.md`, Testnet as it runs, says how to
  undo it).
- **Deposits.** A deposit address derives from the seed (BIP-44
  `m/44'/60'/(1000 + account)'/0/index`, testnet `1'`); the relayer moves its USDC into the
  wallet's Railgun balance on one EIP-3009 `receiveWithAuthorization`, from an account of its own,
  for a floor plus `fee_bps`.
- **Sweeps** build with `OvkPolicy::Discard` (`Wallet::sweep`): both sides hold the joint account's
  viewing key. The app must sweep to a never-used address each time (`docs/local/android.md`,
  section 7).
- **Terms hash.** The contract stores only `hashTerms(terms)`; every call on a swap passes its
  `Terms` and reverts `WrongTerms` on a mismatch. Rust's one definition is
  `zecswap_core::Terms::hash`, and every relayer request carries `terms`.
- **Contract.** Swap ids and single-use maker shares are bound to the maker's address, and a
  reverse id (`reverseSwapId`) is never a forward one. `deposit` credits only what arrives. A
  payout shields to the note committed at open from a per-swap EIP-1167 vault whose returns can be
  rescued. `claim` is a pull payment, and the revealed share is stored (`Swap.secret`), so nothing
  reads event logs.
- **Maker state.** A swap is recorded before its `open`, a sweep before its broadcast. Each swap
  stores its token and `t0`; share indices come from the clock (`next_share_index`); `check_swaps`
  refuses live swaps opened under another key; `cancel_after` counts from the open landing;
  `max_awaiting_deposit` caps swaps waiting on users.
- **Networks.** `network = "testnet" | "mainnet"` in each service's config fixes the Zcash network,
  chains, Railgun, USDC and lightwalletd, and each refuses another's. The pinned crates schedule
  NU7 on testnet alone: move to crates that schedule it on mainnet before it activates there.

## Gotchas

- Sepolia repriced new state in October 2026: deploying ZecSwap takes ~28M gas and `open` ~360k.
  `forge script` simulates the old prices and runs out of gas: deploy with `cast send --create` at
  the node's estimate (`deploy/testnet/deploy-windows.sh`). The live suite's anvil fork keeps the
  old prices.
- anvil's fork errors quote the upstream URL, Alchemy key included: filter URLs out of anything
  printed from a fork (`sed -E 's#https?://[^ "]+#<url>#g'`).
- Only the owner's Alchemy RPCs, never public ones (the owner's rule): the droplet's services use
  one key, local work `.env.testnet`'s. On `429 Monthly capacity limit exceeded`, stop and ask.
  The 30M CU monthly cap is per account, shared by the maker, relayer, `.env.testnet` and the
  Android dev builds. Flashbots Protect is the one exception, for sending: mainnet deposits, and
  mainnet reveals on builds from main.
- Alchemy's free tier caps `eth_getLogs` at 10 blocks, and load-balanced RPCs read a block behind.
  Railgun's SDK scans logs, so `balance.cjs` and `send.cjs` need the Infura URLs.
- `blake-hash` (BLAKE-512) panics on ARM, phones included; `zecswap-railgun` uses the portable
  `bloock-blake-rs`.
- Railgun accepts any ciphertext, and a note its receiver can't decrypt is lost: never change
  `zecswap-railgun`'s note code without `engine/check.cjs` passing.
- In Foundry tests, `vm.prank` applies to the next external call, views included: compute a note
  commitment before pranking.
- Zcash testnet blocks sometimes land every few seconds, so confirmation rules fire early.
- A wallet with no accounts would scan ~300k blocks of Ironwood's last shard; `Wallet::sync` skips
  until an account exists. A CLI wallet that never synced fails its deposit with "Must scan blocks
  first": run `status` first.
- Local anvil must run `--block-time 2`: the maker's clock is chain time.
- For about 15 minutes after a contract deploys, the finalized block predates it, and the maker
  logs `ABI decoding failed` reading settled swaps there. Harmless.
- The CLI resumes the `swap.json` in its data dir, and one in an older format stops it
  (`missing field t0`): rename a finished one `swap.json.finished-<date>`.
- Rust is pinned to 1.92, the Android SDK's Rust backend's. Hence alloy 1.8 with
  `alloy-primitives`/`alloy-sol-type-parser` at `~1.5`: newer alloy needs Rust 1.94, and
  alloy-primitives 1.6+ clashes with zcash_transparent's pre-release `digest`.
- `zcash_client_sqlite` needs `transparent-inputs` because `zcash_client_backend/pczt` turns it on.
- Don't reintroduce an orchard fork or SDK patch: signing goes through pczt's public Signer.
- A maker store from an older maker lacks columns the swaps need, and the maker refuses to start on
  it: a new deployment's maker gets a fresh store (share indices from the clock never repeat).
- Never sign EVM transactions through `ProviderBuilder::new()`'s default fillers: its nonce cache
  advances on failed sends. `evm::signing_provider` shows the safe stack.
- Async closures (`AsyncFnMut`) held in a future break `Send` for spawned tasks; poll with plain
  predicates instead.
- `terms_tests` runs within 256-384 KiB of the 2 MiB test-thread stack in debug builds: its frame
  holds a slot for every relayer future it awaits. Larger send futures overflow it (the binary
  aborts and its anvils keep any pipe open): `submit_held` boxes each attempt and `send_once` its
  send, so keep send state there.
- The agent shell is zsh: `grep` is a wrapper function (use `command grep`), `$vars` don't
  word-split (`${=var}` does), and unquoted globs in flags fail (`--include='*.rs'`).

## Conventions

- Commits: author `chinmaygopal931 <chinmayg015@gmail.com>` (the git config here already; never a
  work address), a single-line lowercase conventional subject with a scope (`fix(maker): …`), no
  Claude/Anthropic attribution anywhere, and explicit paths when staging (no `git add -A`).
- Code: sparse comments (only non-obvious whys), no duplication, finish what you touch, keep
  clippy and fmt clean. Explain behaviour to the owner in plain language, not symbol names.
- Deliver reports in the terminal, not as hosted pages.
