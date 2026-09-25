# ZecSwap: handoff for the next agent

Non-custodial swaps of shielded ZEC (Ironwood pool) for USDC on Base. The deposit address's
spend key is split `±(e + z)`: the maker holds `e`, the user holds `z`. A Base contract pays
out against whichever half is revealed, checking it on Pallas. Target client is the Android
wallet `~/dev/zapp/zapp-android` (iOS later).

Read `docs/local/plan.md` next (gitignored, internal): the decisions that change the original
design, the task tracks, open questions. For the Android client, `docs/local/android.md` is the
handoff: what to build where, the exact flow, the SDK traps, test vectors, a testnet maker. The original design and threat model are in
`~/dev/zapp/ios-zapp/docs/local/shielded-swap-plan.md`; the plan says where it no longer holds.

## Where things stand (2026-09-25)

- Built and tested: Rust core, contracts, Zcash/Base adapters, maker service, reference user
  client, test CLI, and a live end-to-end suite. Offline: 22 Rust tests (crypto, policy and
  pricing edge cases; four build, prove and sign a full Ironwood spend from a joint address;
  one runs the Base client on a local anvil) and 48 contract tests incl. 7 invariants.
- **All six live scenarios passed in one run** on Base Sepolia + Zcash testnet after a
  hardening review, maker restart included (`.testnet/e2e-hardened.log`). The same run on the
  pre-review code (`.testnet/e2e-baseline.log`) failed `never-claimed` and `abandoned-claim`
  on the nonce-gap bug the review fixed; see "Recent fixes".
- **Next step: the Android client (plan Track D). Start with `docs/local/android.md`**, which
  was researched against zapp-android, SDK 3.2.1 and zappMessaging. Its first tasks are the
  JNI crate and a testnet spike of the SDK refund sweep, the riskiest part.
- Not started: maker hardening (Track B), audit (Track A).
- Git: `main` tracks `github.com/JustZappIt/zecSwap` (private). Commit and push only when the
  owner asks. `docs/local/` (the plan and the Android handoff) is gitignored and lives only on
  the owner's machine, as do `.env.testnet` and `.testnet/`.

## Layout

| Path | Role |
|---|---|
| `crates/zecswap-core` | Pure crypto: shares, proofs of knowledge, joint account (UFVK/UA), seed-derived keys, PCZT signing with the combined key |
| `crates/zecswap-chain` | I/O adapters: Zcash light wallet over lightwalletd (sync, joint accounts, pay, sweep) and the Base contract client |
| `crates/zecswap-api` | The quote API's wire types (serde), shared by the maker and the client; the Kotlin port's spec |
| `crates/zecswap-client` | The user side, step by step (open → verify on-chain → deposit → claim/withdraw, or refund key); the Android driver should mirror it |
| `crates/zecswap-maker` | Maker service: quote API (axum), SQLite store, watchtower; `policy.rs` is the pure decision function |
| `crates/zecswap-cli` | Testnet wallet + user CLI (`init`, `status`, `send`, `swap`) |
| `crates/zecswap-e2e` | Live suite: `src/env.rs` (setup, in-process makers), `src/scenarios.rs` |
| `contracts` | Foundry: `src/ZecSwap.sol`, `src/Pallas.sol`, tests, `script/Deploy.s.sol`, Rust-generated vectors in `test/vectors` |
| `scripts/e2e-testnet.sh` | Runs the live suite |

## Commands

```sh
cargo test --workspace                                  # offline tests, plus the Base client on a local anvil (the live suite skips itself)
cargo clippy --workspace --all-targets; cargo fmt --all
(cd contracts && forge test)                            # FOUNDRY_DISABLE_NIGHTLY_WARNING=1 hides the nightly banner
cargo build --release -p zecswap-core --example evm_vectors && (cd contracts && FOUNDRY_PROFILE=ffi forge test)
scripts/e2e-testnet.sh [happy no-deposit underpaid silent-maker never-claimed abandoned-claim]
cargo run -p zecswap-cli -- --data-dir .testnet/user status   # the funded test wallet
```

A full live run takes about 50 minutes (the refund scenarios wait for `t1`). Start it in the
background and read the log; the owner does not want turns spent waiting on it.

## Testnet resources (never print or commit secrets)

- `.env.testnet` (gitignored, 0600): `MAKER_PRIVATE_KEY` is the funder/maker account
  `0x09eD1F966745Be18C711C346242c0974DAd7c3e5` on Base Sepolia; `BASE_SEPOLIA_RPC_URL` is the
  owner's Alchemy URL, which the live script passes on as `ZECSWAP_E2E_BASE_RPC`;
  `MAKER_ROOT_SECRET`. The owner tops up ETH and TAZ when asked, and offers a better RPC or a
  Blockscout key if one is needed.
- `.testnet/user`: the funded Zcash testnet wallet (seed + sqlite), address `utest1lvvc…`.
- Each live run deploys fresh contracts and keeps its makers' data and root secrets in
  `target/zecswap-e2e/<timestamp>/`.

## Recent fixes worth knowing (all tested)

- The contract stores the revealed share (`Swap.secret`); nothing reads event logs.
- `claim` is a pull payment; the payout is withdrawn separately.
- A lapsed lock gives the other side the next turn; there is no "one lock each" deadlock.
- The watchtower keeps acting on Base when a Zcash sync fails; it only skips "no deposit" cancels.
- Clients wait for RPC lag; sends whose estimate failed are retried; every connection has
  timeouts and keepalives.
- Hardening review (2026-09-25; plan "What changed" 11): every Base send reads its nonce from
  the chain (alloy's default cache left a gap after any failed send and stalled every later
  one); the maker records a swap before `open` and a sweep before broadcasting, and settles a
  swap that never opened once `t1` passes; the client reveals `z` only under a claim lock with
  `CLAIM_MARGIN` left, resumes a claim from any interruption, and refuses a `t0` more than two
  hours out; the wallet stores a transaction (`pay`, `sweep`) and `broadcast` sends it; the
  quote API's wire types live in `zecswap-api`; the maker checks its timing against the
  contract's lock at startup.

## Gotchas

- Alchemy's free tier caps `eth_getLogs` at 10 blocks. Load-balanced RPCs read a block behind.
- Zcash testnet blocks sometimes land every few seconds, so confirmation rules fire early.
- A wallet with no accounts would scan ~300k blocks of Ironwood's last shard; `Wallet::sync`
  skips until an account exists.
- Local anvil must run `--block-time 2`: the maker's clock is chain time.
- Rust is pinned to 1.92, the version the Android SDK's Rust backend uses. Hence alloy 1.8 with
  `alloy-primitives`/`alloy-sol-type-parser` at `~1.5`: newer alloy needs Rust 1.94, and
  alloy-primitives 1.6+ clashes with zcash_transparent's pre-release `digest`.
- `zcash_client_sqlite` needs `transparent-inputs` because `zcash_client_backend/pczt` turns it on.
- Don't reintroduce an orchard fork or SDK patch: signing goes through pczt's public Signer.
- Never sign Base transactions through `ProviderBuilder::new()`'s default fillers: its nonce
  cache advances on failed sends. `base::signing_provider` shows the safe stack.
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
