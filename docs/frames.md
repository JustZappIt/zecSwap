# Sponsored claims through frame transactions (devnet only)

A hackathon proof of concept for ETHGlobal Mumbai (November 2026): the user's wallet sends its own
claim, straight to an alternative mempool, and a sponsor contract pays the gas. No relayer takes
part, and the address that claims never holds ETH. It runs only on the Ethereum Foundation's
EIP-8141 devnet, where frame transactions exist: not on Sepolia, Base Sepolia or mainnet, and it
is not merged into main.

## Why

Today a user with no ETH can't send their own claim, so our relayer sends it, through Flashbots
Protect on mainnet. A relayer can censor or go down. EIP-8141 frame transactions let a contract pay
a transaction's gas, and ERC-8421's alternative mempool admits such transactions under rules a
payer can meet. With both, the wallet sends the claim itself, and a contract nobody can switch off
pays for it. This is an alternative to the relayer, not a replacement.

## The claim

The claim is a frame transaction from the swap's own key, the per-swap key the Railgun route
already uses as the swap's `user`. It's fresh for every swap and never funded:

| Frame | Mode | Calls | Gas limit (execution + state) |
|---|---|---|---|
| 0 | VERIFY, approves execution | the swap's key itself (an account with no code runs EIP-8141's default check of the signature) | 20,000 |
| 1 | VERIFY, approves payment | `ClaimSponsor.sponsor(id, terms)` | 60,000 |
| 2 | SENDER | `ZecSwap.claim(id, terms, z)` | 400,000 + 300,000 |
| 3 | SENDER | `ZecSwap.withdraw(token, amount, to)` | 100,000 + 150,000 |

The key, not the sponsor, sends it: on the account route `claim` credits `terms.user`, and only
`terms.user` can `withdraw`. The contract is unchanged. The claim and the withdrawal are separate
frames, never one atomic batch: a batch that failed would undo the claim with its share already on
chain, while a failed withdrawal only leaves the amount credited.

The limits are fixed, never estimated, because an estimate would show a node the share before the
claim goes out. The devnet measured 100, 29,891, 194,468 and 36,247 execution gas. The claim
creates two storage slots, the share and the credit, at 97,920 state gas each, though its receipt
shows one: the withdrawal empties the credit, and the node takes that refund off the claim frame's
receipt afterwards. Limits copied from that receipt would stop the claim halfway, its share public.

## The sponsor

`frames/contracts/src/ClaimSponsor.sol` approves paying for a transaction only if:

- the swap exists with exactly these terms, and is open or ready (not claimed or refunded);
- it is a forward swap of our maker's, paying an account;
- the sender is the swap's own key, on its first transaction;
- the transaction can cost at most 0.005 ETH (its gas limits times its fee cap).

It checks neither the share nor the window. Checking the share costs about 165,000 gas, and the
alternative mempool, like EIP-8141's own, gives a transaction's checks 100,000 in all: the devnet
lifted that cap on 2026-11-06, but every node and builder runs the checks before anyone pays them,
so we stay well inside it at 32,791 with the signature. The window needs the clock, which no check
may read. A wrong or late share makes the claim frame revert, so each real swap costs the sponsor
at most one transaction, which only the swap's own key can send. Requiring our maker keeps anyone
from opening swaps against themselves to drain it a transaction at a time. It has no owner: nobody
can withdraw its ETH or unlock its stake while a claim waits on it.

## The alternative mempool

The sponsor reads ZecSwap's storage while it checks. EIP-8141's own mempool refuses that; ERC-8421's
admits it from a payer that has staked. On the devnet, the same signed claim was:

1. refused by the canonical mempool (ethrex): `StorageReadNonSender`;
2. refused by the alternative mempool while the sponsor was unstaked: "unstaked payer accessed
   [ZecSwap] slot [the swap's]";
3. admitted once the sponsor had staked 0.012 ETH for a day, and mined: [the probe's
   claim](https://explorer.altmempool.wtf/tx/0x984442a2a5114d5558896139aec5e3e0aea1a6d9315578b3d42745363a89094b)
   (pick the explorer's "Miner" instance).

## A real swap

On 2026-11-06 the CLI swapped real testnet ZEC for 1 tUSD against a local maker, end to end: the
deposit (Zcash `9356608a…`), the maker's ready, then the claim, sent by the swap's own key and paid
for by the sponsor ([`0xf0465fe7…`](https://explorer.altmempool.wtf/tx/0xf0465fe7fa2054feac2d3d63491b7674f1aa5f3fa080f882a9c6305cbc89bc52),
mined 10 seconds after the miner's mempool node admitted it), and the maker's sweep of the ZEC 20
seconds later (Zcash `cae3d48e…`). The claiming address holds the 1 tUSD and never held ETH.
`deployments/frames-devnet.json` records every transaction.

## What the wallet does

The share is public the moment the claim is sent: the alternative mempool gossips it, and nothing
keeps a claim that lands late out of a block. A seen share hurts only if the claim misses its
window; then the maker holds the whole key, takes the ZEC, and refunds the USDC. So the client
(`Route::Frames` in `crates/zecswap-client/src/user.rs`):

1. judges the claim's window as every claim does, on one read of the chain;
2. sends only with 15 minutes of the window left, or if it lasts for good;
3. sends one transaction, signed deterministically, so a run that resumes sends the same bytes,
   and sends it again every 5 minutes, the time a mempool node keeps one, until the swap shows
   claimed;
4. never estimates or simulates with the share;
5. watches the swap, not a receipt (alloy can't decode a type 0x06 receipt), then checks nothing
   stays credited in the contract.

## The threat model

- Unchanged: the contract's windows (at any moment at most one side may reveal), every client
  judgement, and the maker, which reads the share from storage however the claim arrived.
- New: the pending share is public. That is safe early in the window, and dangerous only if the
  claim is kept out of blocks until `t1`. On this devnet one mempool node and one miner include
  everything, so whoever runs them could censor. On mainnet that needs builders to include
  ERC-8421 transactions widely.
- The sponsor may pay for one doomed transaction per real swap, at most 0.005 ETH. Someone flooding
  pending transactions against its balance could get a claim refused: fund it well.
- Mainnet would need EIP-8141 live, the expiry verifier frame (a deadline of `t1` less a margin,
  turning a late claim into an invalid transaction instead of a public share), wide inclusion of
  ERC-8421 transactions, the sponsor repaid on chain, and the relayer through Protect as the
  fallback.

## Known gaps

It's a proof of concept:

- A withdrawal that fails leaves the amount credited in the contract to a key with no ETH to send
  another transaction, and the sponsor pays only a key's first.
- The signed claim isn't stored: a resumed run rebuilds the same bytes only with the same sponsor
  and recipient.
- The maker's cost recorder can't read a frame transaction's receipt, and logs a warning as it
  retries; settling is unaffected.
- The sponsor is simply funded: nothing repays it on chain.

## Running it

```sh
cd frames && npm ci
node scripts/deploy.mjs          # tUSD, ZecSwap (30-minute and 1-hour windows) and the sponsor, funded and staked
node scripts/probe.mjs --deployment   # a test vector's swap claimed through the sponsor
(cd contracts && forge test)     # the sponsor's tests
node scripts/vectors.mjs > ../crates/zecswap-chain/tests/frame-vectors.json   # the encoder's known answers
```

`.env.frames` holds the devnet keys (`FRAMES_DEPLOYER_KEY`, `MAKER_PRIVATE_KEY`) and the maker's
secrets, never committed; fund the keys from the devnet's faucet (`skandha_explorer_faucet`). Then
run a maker from `frames/maker.toml.example`, and swap with the CLI:

```sh
cargo run -p zecswap-cli -- --data-dir .testnet/frames-user swap --maker http://127.0.0.1:8899 \
  --rpc https://altmempool.forshtat.com --contract <ZecSwap> --token <tUSD> \
  --frames https://altmempool.forshtat.com/rpc --sponsor <ClaimSponsor>
```

`deployments/frames-devnet.json` records the live deployment, with block 1's hash: the devnet is
reset now and then, and `deploy.mjs` deploys again over a manifest whose hash no longer matches.

## The devnet

| What | Where |
|---|---|
| ethrex (chain 1337, 6-second blocks) | `https://altmempool.forshtat.com` |
| The miner's mempool node | `https://altmempool.forshtat.com/rpc` |
| The gateway mempool node | `https://altmempoolgateway.forshtat.com/rpc`: had no peers on 2026-11-06 and passed nothing on, so we send to the miner's |
| Explorer | `https://explorer.altmempool.wtf/` |
| Staking registry | `0xd51c04d7a0b01124e84de152f4e56d9eecb22d2f` |

The frame encoding follows `@forshtat/hegota-support`'s `ethrex-eip8141-tracer` profile, which
differs from the EIP text: `crates/zecswap-chain/tests/frame-vectors.json` pins it.
