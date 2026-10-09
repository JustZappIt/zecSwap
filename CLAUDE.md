# ZecSwap: handoff for the next agent

Non-custodial swaps of shielded ZEC (Ironwood pool) for USDC. The deposit address's spend key is
split `±(e + z)`: the maker holds `e`, the user holds `z`. An EVM contract pays out against
whichever half is revealed, checking it on Pallas. The payout goes into the user's private
Railgun balance on Ethereum, with no user account on-chain; withdrawing from it to any Ethereum
address comes later. The contract can also pay an account (the earlier Base route, still tested),
but the owner shelved that route for the app on 2026-09-25. Target client is the Android wallet
`~/dev/zapp/zapp-android` (iOS later).

Read `docs/local/plan.md` next (gitignored, internal): the decisions that change the original
design, the task tracks, open questions. `docs/local/private-usd.md` is the Railgun route's
brief, with what was verified about Railgun (section 13). For the Android client,
`docs/local/android.md` is the handoff: what to build where, the exact flow, the SDK traps, test
vectors, a testnet maker. The original design and threat model are in
`~/dev/zapp/ios-zapp/docs/local/shielded-swap-plan.md`; the plan says where it no longer holds.

## Where things stand (2026-09-25)

- Built and tested: Rust core, contracts, Zcash/EVM adapters, maker service, relayer, Railgun
  keys and notes, reference user client, test CLI, and a live end-to-end suite. Offline: 31 Rust
  tests (crypto, policy and pricing edge cases; four build, prove and sign a full Ironwood spend
  from a joint address; Railgun keys and notes against Railgun's engine; a Railgun swap settled on
  Rust signatures on a local anvil) and 69 contract tests incl. 9 invariants, plus 3 fork tests
  against the real Railgun and USDC on Ethereum mainnet.
- **All six Base scenarios passed live** on Base Sepolia after the hardening review
  (`.testnet/e2e-hardened.log`). **All nine, the three Railgun ones included, passed live on a
  fork of Ethereum Sepolia** with the real Railgun contract and the live Zcash testnet, at 2
  confirmations in 24 minutes (`.testnet/e2e-fork-fast.log`): payouts shielded into the user's
  `0zk` wallet, a resumed payout, and every refund path.
- **All nine passed live on Ethereum Sepolia itself** (`.testnet/e2e-railgun-sepolia.log`), and
  Railgun's own wallet SDK (`balance.cjs`) shows both payouts in the user's `0zk` wallet as
  Spendable: they cleared Railgun's screening.
- **Next: the Android client, Railgun route only: `docs/local/android.md` section 9**
  (milestones M1–M6). First the JNI library and a testnet spike of the SDK refund sweep; the
  in-app Railgun wallet (M5: Railgun's TypeScript SDK in a hidden WebView, proofs by `mopro`;
  not nodejs-mobile, which is unmaintained and lacks 16 KB page support) runs alongside. New app code is named `AtomicSwap` (zapp-android's `ZecSwap*` types are NEAR's).
- Not started: maker hardening (Track B), audit (Track A), the onramp (brief section 3.3).
- Git: `main` tracks `github.com/JustZappIt/zecSwap` (private). Commit and push only when the
  owner asks. `docs/local/` (the plan and the Android handoff) is gitignored and lives only on
  the owner's machine, as do `.env.testnet` and `.testnet/`.

## Layout

| Path | Role |
|---|---|
| `crates/zecswap-core` | Pure crypto: shares, proofs of knowledge, joint account (UFVK/UA), seed-derived keys incl. the per-swap auth key, EIP-712 signing, PCZT signing with the combined key |
| `crates/zecswap-railgun` | Railgun keys and `0zk` addresses from the seed, the encrypted shield note a payout goes to, and transaction outputs as their receiver reads them (a relayer's fee notes); `engine/` checks it against Railgun's own engine and wallet SDK (Node), and `send.cjs` proves sends with the SDK for the live suite |
| `crates/zecswap-chain` | I/O adapters: Zcash light wallet over lightwalletd (sync, joint accounts, pay, sweep) and the EVM contract client (`evm`) |
| `crates/zecswap-api` | The quote and relayer APIs' wire types (serde), shared with wallets; the Kotlin port's spec |
| `crates/zecswap-client` | The user side, step by step (open → verify on-chain → deposit → claim, or refund key), paid to an account or into Railgun; the Android driver should mirror it |
| `crates/zecswap-maker` | Maker service: quote API (axum), SQLite store, watchtower; `policy.rs` is the pure decision function |
| `crates/zecswap-relayer` | Sends the transactions of users with no account on the chain, on their signatures, for one token and maker, and with `[railgun_sends]` wallets' private Railgun sends and withdrawals as their broadcaster (`docs/railgun-sends.md`); never run by a maker |
| `crates/zecswap-prices` | USD prices of ZEC, USDC and ETH: `Feed`, the latest from providers in order (the maker's quotes, the relayer's gas-priced fees), and `History`, Alchemy's five-minute candles for costs recorded after the fact |
| `crates/zecswap-tokens` | Privacy Pass tokens (RFC 9577/9578), good on their UTC day: client blinding, issuer signing, and (`server`) the gate the maker's accepts spend them through, with the maker's key for handing them back |
| `crates/zecswap-issuer` | Signs each device a day's tokens (one per accept), blind, counted by the install's Android Keystore key, attested up to Google's root (`android.rs`, `x509.rs`; `docs/tokens.md`) |
| `crates/zecswap-cli` | Testnet wallet + user CLI (`init`, `status`, `send`, `swap`, `swap --relayer` for Railgun) |
| `crates/zecswap-e2e` | Live suite: `src/env.rs` (setup, in-process makers, relayer and token issuer), `src/scenarios.rs` |
| `contracts` | Foundry: `src/ZecSwap.sol`, `src/ShieldVault.sol` (per-swap Railgun payout vaults), `src/Pallas.sol`, `src/Token.sol`, tests incl. `test/fork`, `script/Deploy.s.sol`, Rust-generated vectors in `test/vectors` |
| `scripts/e2e-testnet.sh` | Runs the live suite |

## Commands

```sh
cargo test --workspace                                  # offline tests, plus the EVM client on a local anvil (needs `forge build`; the live suite skips itself)
cargo clippy --workspace --all-targets; cargo fmt --all
(cd contracts && forge test)                            # FOUNDRY_DISABLE_NIGHTLY_WARNING=1 hides the nightly banner
(cd contracts && ETH_RPC_URL=… forge test --match-path test/fork/RailgunFork.t.sol --isolate -vv)   # real Railgun + USDC, with gas
cargo build --release -p zecswap-core --example evm_vectors && (cd contracts && FOUNDRY_PROFILE=ffi forge test)
scripts/e2e-testnet.sh [scenario ...]                   # Base Sepolia
scripts/e2e-testnet.sh --railgun [--fork] [scenario ...] # Ethereum Sepolia with Railgun; --fork on a local anvil fork of it
cargo run -p zecswap-cli -- --data-dir .testnet/user status   # the funded test wallet
cargo run -p zecswap-client --example vectors           # known answers for the Android port
# Railgun's engine and wallet SDK (cd crates/zecswap-railgun/engine && npm ci):
node vectors.cjs > ../tests/engine-vectors.json         # regenerate the Rust tests' known answers
cargo run -p zecswap-railgun --example note | node crates/zecswap-railgun/engine/check.cjs
ETH_SEPOLIA_RPC_URL=… node balance.cjs <seed-file> [from-block]   # a wallet's Railgun balance by screening status
```

Deposits count after 2 confirmations in the live suite (`ZECSWAP_E2E_CONFIRMATIONS`, default
2), with deadlines to match; with 10, the wallets' default, a full run takes about 50 minutes.
Start it in the background and read the log; the owner does not want turns spent waiting on it.
Never edit `scripts/e2e-testnet.sh` while it runs: bash reads it as it goes.

## Testnet resources (never print or commit secrets)

- `.env.testnet` (gitignored, 0600): `MAKER_PRIVATE_KEY` is the funder/maker account
  `0x09eD1F966745Be18C711C346242c0974DAd7c3e5` (Base Sepolia ETH; none yet on Ethereum
  Sepolia); `BASE_SEPOLIA_RPC_URL` and `ETH_SEPOLIA_RPC_URL` are the owner's Alchemy URLs (one
  key; it also serves `eth-mainnet`, for fork tests); `MAKER_ROOT_SECRET`. The owner tops up ETH
  and TAZ when asked, and offers a better RPC or a Blockscout key if one is needed.
- Railgun on Ethereum Sepolia: proxy `0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea`, screening
  after 60 s; on mainnet `0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9`. The test wallet's
  Railgun wallet (seed index 0) is `0zk1qyjqfvr4…`.
- `.testnet/user`: the funded Zcash testnet wallet (seed + sqlite), address `utest1lvvc…`.
- Each live run deploys fresh contracts and keeps its makers' data and root secrets in
  `target/zecswap-e2e/<timestamp>/`.

## Recent fixes worth knowing (all tested)

- Profitability (2026-10-08, branch `feature/profitability`): the price engine asks
  `[pricing.market] providers` in order (`coinmarketcap`, `alchemy`; required; keys
  `ZCASH_CMC_KEY`, `ALCHEMY_API_KEY`), so quotes pause only while every one fails; the
  monitor's `pricing.source` names the provider and `pricing.providers` each one's error. The
  maker records what each swap earned and cost: its quote's prices (`quote_prices`), every
  Ethereum transaction on it from one receipt read (`evm_costs`: sender, gas, effective price,
  the token paid to the sender, i.e. a relayer's fee), its own sends journaled first
  (`sent_transactions`, so a reverted one, which emits no event, still counts), and the exact
  fee of each of its Zcash sends (`zcash_costs`, `Wallet::fee`), each valued in USD when it
  happened: `live` from its own price within five minutes, else `history` from Alchemy's
  five-minute candles. `GET /v1/monitor/economics?since=` exports it and zapp-dashboard's
  `/bridge/profit` charts it. The relayer keeps the same for the private sends and unshields it
  sponsors (`send_costs` in its journal: gas from each receipt, kind and fee read back from the
  send's journaled bytes, a reverted one earning nothing), at `GET /v1/monitor/sends`; the live
  `railgun-send` checks it. Both value history through `crates/zecswap-prices` (Alchemy's
  five-minute candles). A sponsored send burns about 1.07M gas (~$0.32 at 0.12 gwei, $2.65 at
  1 gwei), so a fixed fee can't follow gas: with `providers` (top level of the relayer's config)
  every relayer fee follows it (`src/pricing.rs`). A send's terms add `feePerUnitGas` (USDC base
  units per 10^18 wei, `fee_margin_bps` included) and `feeExpiresAt`; `fee` is the floor. The app
  prices a send with the SDK's broadcaster estimate and `calculateBroadcasterFeeERC20Amount`,
  never under the floor (`engine/send.cjs`, `priced`); the relayer checks fee × 10^18 ≥ its own
  gas estimate × the gas price it pays × the lowest rate it quoted in ten minutes, and answers
  `503` while it can't price gas. Swap fees (2026-10-09; the owner wants each swap profitable,
  with no minimum size): a payout's fee is the larger of `fee` and `fee_gas` (claim lock, claim
  and payout, 1.65M on Sepolia) at the gas price and rate now, the reverse funding's likewise
  (`[reverse_funding] fee_gas`, 2.06M); terms quote both with a top-level `feeExpiresAt`, and the
  relayer takes any fee at or above the lowest it quoted in the hour (`SWAP_FEE_VALIDITY`), or
  the floor while unpriced, so no claim waits on a price. The maker charges its own gas and Zcash
  fee in every quote (`[pricing.costs]`: `forward_gas` 412k on Sepolia, `reverse_gas` 362k,
  `zcash_fee_zat`, `margin_bps`) at the chain's gas price and the quote's prices, on top of the
  spread; quotes say so (`networkCost`) and `quote_costs` keeps it. Sepolia's test token logs no
  `Transfer`s, so the ledger reads a relayer's fee from the escrow's `PaidOut`/`Rescued` and a
  funding's Relay Adapt calldata too (`evm::paid_to`). The shared `crates/zecswap-prices` `Feed`
  (CoinMarketCap then Alchemy) serves both maker and relayer; its `stand-in` feature is the
  providers' test double. New tables only, so existing stores and journals open and backfill.
  Deploy the dashboard first (an older one rejects `source: alchemy`). The app records a swap's
  relayer fee when it quotes and caps it (`MAX_RELAYER_FEE`); it signs that fee at the claim.
- Railgun broadcaster (2026-10-08, branch `feature/railgun-broadcaster`): `POST
  /v1/railgun/transact` sends a wallet's own proved `transact` (private send or withdrawal) for
  a fee note to the relayer's 0zk address, read as Railgun's public broadcasters read it
  (`evm/railgun.rs`); any chain, the proxy read from the contract. Every send is journaled
  (SQLite) before its broadcast, so the same bytes never send twice; answers are `200`,
  `400 rejected` (nothing sent, ever), `409 alreadySpent`, `5xx` (post again). The live suite's
  `railgun-send` proves with Railgun's own SDK (`engine/send.cjs`); on a fork it cuts the SDK's
  quick sync at the fork block and turns screening off. The relayer's Sepolia Railgun seed and
  mnemonic are in `.env.testnet`. Open: on live Sepolia a relayed send's outputs never clear
  screening, since Railgun's indexer breaks its txid verification-hash chain at index 4188 and
  SDK wallets can't prove their own sends; forwarding the SDK's pre-send POIs would fix it.
- Monitoring (2026-10-08, branch `feature/monitoring`): the maker's `/v1/monitor` is schema 2
  and reads nothing from the chain (the dashboard, `~/dev/zapp-dashboard`, reads swap states and
  balances on its own RPC through Multicall3); it adds per-swap accept/open/settle times, block
  times of Ethereum events, token outcomes by day (walk-aways = wasted tokens), the deposit cap
  and gas alert accounts, and `/v1/monitor/swaps/{id}`. The issuer and relayer have their own
  `/v1/monitor` (`ISSUER_MONITOR_TOKEN`, `RELAYER_MONITOR_TOKEN`): day totals and refusals by
  reason, never a device; relayed transactions by outcome and their gas, read after the reply.
  All three share one gateway allowance; Alloy ships the issuer's journal. The dashboard pins
  the contract in Vercel's `BRIDGE_TESTNET_CONTRACT`: update it with every contract redeploy.
- PR #8 review (2026-10-06), each fix with a test that fails without it:
  - Contract: a reverse escrow's id is `reverseSwapId` (`keccak256(abi.encode(user, makerKey,
    true))`), never a forward id, so a maker can't open a user's verified forward swap as a
    reverse escrow and take away its claim after `t0`; `deposit` credits only what arrives;
    an `open` paying into Railgun above uint120 is refused.
  - Relayer: serves only its `token` and `maker`, on every route; refuses a payout whose fee
    leaves nothing to shield; checks Railgun takes the payout before revealing in a claim.
  - Maker: each swap stores its token (terms never follow the config); share indices come from
    the clock, so a restored backup can't reissue one; `check_swaps` refuses to start on live
    swaps opened under another key or root secret; settled swaps are re-read for a day, then
    archived and their wallet accounts dropped; an accept reads its quote and the chain clock
    before taking the quote or importing an account; `max_awaiting_deposit` caps swaps waiting
    on users.
  - Spam: with `[tokens]`, each accept spends a Privacy Pass token (`docs/tokens.md`).
    The issuer's `android-key` mode checks Android hardware key attestation (one key per
    install), and the hosted testnet issuer runs it since 2026-10-08; `insecure-test` starts
    only with `allow_insecure = true`.
- Tokens count walk-aways (2026-10-07): a device walks away from at most `tokens_per_day` swaps
  a day (3) and makes as many as it pays into. The gate holds an accept's token while it runs
  and keeps it only once the quote is taken (`Spend::keep`): a refused accept leaves it
  spendable. The accept body's `tokenRequest`, blinded under the maker's `return_key` (in
  `[tokens]`, required; `/v1/info` shows it, the app pins it), is signed once the user pays in
  in full (forward, in the maker's wallet view) or the escrow is funded (reverse), or when a
  forward `open` never landed; `GET /v1/swaps/{id}` (new) and `/v1/reverse/swaps/{id}` serve the
  blind signature as `tokenReturn`. Challenges carry the UTC day in their redemption context,
  so tokens die at midnight and the spent store keeps only today's. The client spends the
  issuer's tokens before returned ones and drops earlier days'.
- Attestation and the gateway (2026-10-08): the issuer reads a certificate's `TRUE` written as
  1 (a OnePlus StrongBox writes it so) and Google's status list's decimal serials (most of
  them), and `zecswap-issuer-status.timer` refreshes that list. Only the app's testnet builds on
  a locked phone get tokens from the hosted issuer: not emulators, unlocked phones or the CLI.
  `deploy/nginx.conf` gives each kind of request its own allowance, claims and refunds theirs,
  and each swap's status reads one too; nginx refuses with `503` and `Retry-After`, never
  `429`, which from the issuer means a device's tokens for the day are spent. The Worker
  forwards only `Authorization`, `Content-Type` and `Content-Length`.
- Terms hash (2026-10-06): the contract stores only `hashTerms(terms)` of each swap, so `open`
  writes three slots (about 106k gas, from 243k–264k). Every call on a swap takes its `Terms`
  after the id and reverts `WrongTerms` unless they hash to it; one loader (`_load`) does
  this and `test/ZecSwap.terms.t.sol` fails if a function skips it. `Opened` still emits the
  terms. Rust's one definition is `zecswap_core::Terms::hash`; `Settlement::swap(id, &terms)`
  errors `Error::WrongTerms` on a mismatch, and `swap_state` reads the slim state alone. The
  maker persists each swap's token and `t0` (its terms never follow the config) and returns
  `t0`/`t1` in `Accepted`; relayer requests carry `terms`.
- The contract stores the revealed share (`Swap.secret`); nothing reads event logs.
- `claim` is a pull payment; the payout is withdrawn separately.
- A lapsed lock gives the other side the next turn; there is no "one lock each" deadlock.
- The watchtower keeps acting on Base when a Zcash sync fails; it only skips "no deposit" cancels.
- Clients wait for RPC lag; sends whose estimate failed are retried; every connection has
  timeouts and keepalives.
- Railgun route (2026-09-25; brief section 13): swap ids and single-use maker shares are bound
  to the maker's address, so a copied `open` on a public mempool can't block the real one; a
  Railgun swap's `user` is a per-swap key that signs its claim lock (EIP-712, one lock per
  signature) and its payout (naming the relayer and its fee); the payout shields to the note
  committed at open, from a per-swap EIP-1167 vault that Railgun's "back to origin" returns can
  be rescued from; the client checks Railgun would take the payout before revealing.
- The maker's "no deposit" clock (`cancel_after`) starts when `open` lands, not at accept: on
  Ethereum Sepolia nine queued opens landed two minutes after acceptance, and the maker cancelled
  swaps whose deposits were on their way (`.testnet/e2e-railgun-sepolia-queued-opens.log`).
- Hardening review (2026-09-25; plan "What changed" 11): every Base send reads its nonce from
  the chain (alloy's default cache left a gap after any failed send and stalled every later
  one); the maker records a swap before `open` and a sweep before broadcasting, and settles a
  swap that never opened once `t1` passes; the client reveals `z` only under a claim lock with
  `CLAIM_MARGIN` left, resumes a claim from any interruption, and refuses a `t0` more than two
  hours out; the wallet stores a transaction (`pay`, `sweep`) and `broadcast` sends it; the
  quote API's wire types live in `zecswap-api`; the maker checks its timing against the
  contract's lock at startup.

## Gotchas

- Sepolia repriced new state in early October 2026: deploying ZecSwap takes ~30M gas (4.2M
  before) and `open` ~360k (108k on a local fork). `forge script` simulates the old prices and
  runs out of gas: deploy with `cast send --create` and the node's estimate. The anvil fork the
  live suite uses keeps the old prices.
- anvil's fork errors quote the upstream URL, Alchemy key included: filter URLs out of anything
  printed from a fork (`sed -E 's#https?://[^ "]+#<url>#g'`).
- Alchemy's free tier caps `eth_getLogs` at 10 blocks. Load-balanced RPCs read a block behind.
  Railgun's wallet SDK scans logs, so `balance.cjs` needs an RPC without that cap;
  `https://ethereum-sepolia-rpc.publicnode.com` works.
- `blake-hash` (BLAKE-512) panics on ARM, phones included: its SIMD fallback is unimplemented.
  `zecswap-railgun` uses the portable `bloock-blake-rs`, as railgun-rust does.
- Railgun accepts any ciphertext, and a note its receiver can't decrypt is lost: never change
  `zecswap-railgun`'s note code without `engine/check.cjs` passing.
- In Foundry tests, `vm.prank` applies to the next external call, views included: compute a
  note commitment before pranking, not in the call's arguments.
- Zcash testnet blocks sometimes land every few seconds, so confirmation rules fire early.
- A wallet with no accounts would scan ~300k blocks of Ironwood's last shard; `Wallet::sync`
  skips until an account exists.
- Local anvil must run `--block-time 2`: the maker's clock is chain time.
- Rust is pinned to 1.92, the version the Android SDK's Rust backend uses. Hence alloy 1.8 with
  `alloy-primitives`/`alloy-sol-type-parser` at `~1.5`: newer alloy needs Rust 1.94, and
  alloy-primitives 1.6+ clashes with zcash_transparent's pre-release `digest`.
- `zcash_client_sqlite` needs `transparent-inputs` because `zcash_client_backend/pczt` turns it on.
- Don't reintroduce an orchard fork or SDK patch: signing goes through pczt's public Signer.
- A maker store from an older maker lacks columns the swaps now need, and the maker refuses to
  start on it: a new deployment's maker gets a fresh store. Maker share indices come from the
  clock (`next_share_index`), so a fresh or restored store never reissues one.
- Never sign EVM transactions through `ProviderBuilder::new()`'s default fillers: its nonce
  cache advances on failed sends. `evm::signing_provider` shows the safe stack.
- Async closures (`AsyncFnMut`) held in a future break `Send` for spawned tasks (rustc's
  "implementation of `Send` is not general enough"); poll with plain predicates instead.
- The agent shell is zsh: `grep` is a wrapper function (use `command grep`), `$vars` don't
  word-split, and unquoted globs in flags fail (`--include='*.rs'`).

## Conventions

- Commits: author `chinmaygopal931 <chinmayg015@gmail.com>` (the git config here already; never
  a work address), a single-line lowercase conventional subject with a scope
  (`fix(maker): …`), no Claude/Anthropic attribution anywhere, and explicit paths when staging
  (no `git add -A`).
- Code: sparse comments (only non-obvious whys), no duplication, finish what you touch, keep
  clippy and fmt clean. Explain behaviour to the owner in plain language, not symbol names.
- Deliver reports in the terminal, not as hosted pages.
