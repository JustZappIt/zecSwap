# Private sends and withdrawals through the relayer

A wallet sends privately from its Railgun balance, or withdraws from it to any Ethereum address,
without holding ETH. It proves the Railgun transaction itself, with a fee note to the relayer's
own 0zk address as the first output, and posts it to the relayer, which checks that note and pays
the gas, as Railgun's public broadcasters do. The phone's keys never leave it, no public account
of the phone's appears on the chain, and the relayer can delay a send but not change it: the proof
binds every output.

This is opt-in per relayer (`[railgun_sends]`) and works on whichever chain the relayer's
settlement contract is: the chain ID and Railgun's proxy come from that contract. It sends only
plain `transact` calls to the proxy: no Relay Adapt, so no base-token unshields and no
cross-contract calls.

## Terms

`GET /v1/terms` includes `railgunSends` when the relayer sends them. When it is absent, the
relayer does not; don't fall back to a phone-funded transaction.

```json
{
  "relayer": "0x…",
  "chainId": 11155111,
  "contract": "0x…",
  "fee": "20000",
  "railgunSends": {
    "railgunAddress": "0zk1…",
    "railgunProxy": "0xecfcf3b4ec647c4ca6d49108b311b7a7c9543fea",
    "token": "0x…",
    "fee": "500000",
    "feePerUnitGas": "2729606000",
    "feeExpiresAt": 1791504000,
    "maxGasLimit": 3000000,
    "maxGasPriceWei": "50000000000",
    "maxCalldataBytes": 65536
  }
}
```

| Field | Meaning |
| --- | --- |
| `railgunAddress` | The relayer's own 0zk address: the broadcaster fee recipient. |
| `railgunProxy` | Railgun's proxy, the transaction's `to`. Pin it, with `chainId`, against the SDK's own `NETWORK_CONFIG` for the network before proving. |
| `token` | The token the fee is paid in. |
| `fee` | Base units of `token` the fee note must carry at least (decimal string): the floor. |
| `feePerUnitGas` | Where the relayer prices gas, the rate its fee is held to: base units of `token` per 10^18 wei of gas cost, its margin included (decimal string), as Railgun's public broadcasters quote it. Absent while the relayer can't price gas, or where it prices none: then the fee is `fee` alone. |
| `feeExpiresAt` | With `feePerUnitGas`: until when (Unix seconds) a proof whose fee was worked out at this rate is held to it rather than a later one. Ten minutes from the terms. |
| `maxGasLimit` | The most gas the relayer pays for one send. |
| `maxGasPriceWei` | The highest gas price the relayer pays (decimal string), so the highest `overallBatchMinGasPrice` a proof may set. |
| `maxCalldataBytes` | The largest calldata it takes. |

The top-level `fee` is the swap payout fee and does not apply here.

## Proving

With `@railgun-community/wallet` 11.1.0 (engine 9.8.0), `TXIDVersion.V2_PoseidonMerkle`:

- A private send: `generateTransferProof`, then `populateProvedTransfer`. A withdrawal:
  `generateUnshieldProof`, then `populateProvedUnshield`, to the Ethereum address it pays.
- `sendWithPublicWallet = false`.
- The fee: with `feePerUnitGas`, estimate the send as the SDK does for a broadcaster,
  `gasEstimateForUnprovenTransfer` (or `gasEstimateForUnprovenUnshield`) with `feeTokenDetails =
  { tokenAddress: token, feePerUnitGas }` and `sendWithPublicWallet = false`, then
  `calculateBroadcasterFeeERC20Amount(feeTokenDetails, gasDetails)` with that estimate and the
  proof's gas price; pay that, or `fee` if it is more. Without `feePerUnitGas`, pay `fee`.
  `crates/zecswap-railgun/engine/send.cjs` (`priced`) does exactly this.
- `broadcasterFeeERC20AmountRecipient = { tokenAddress: token, amount: <that fee>,
  recipientAddress: railgunAddress }`. The SDK makes it the first output of the first transaction.
- `overallBatchMinGasPrice` no higher than `maxGasPriceWei`; the network's gas price when proving
  is a good choice. The relayer pays the higher of the network's price and this one.
- `populateProved*` takes gas details it only copies into the populated transaction; the relayer
  ignores them. Use `getEVMGasTypeForTransaction(network, false)` (Type 1 on Ethereum) with
  `gasPrice = overallBatchMinGasPrice`.
- No Relay Adapt: not `generateUnshieldBaseTokenProof`, not cross-contract calls.

Post the populated transaction's `to`, `data` and `value` as they are, with the network's chain ID.

## Request

`POST /v1/railgun/transact`, at most 132 KiB:

```json
{
  "chainId": 11155111,
  "to": "0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea",
  "data": "0xd8ae136a…",
  "value": "0"
}
```

Send these four fields and no others: the relayer refuses any other field with `422`. Persist
these exact bytes before posting, and keep the notes they spend locked until an answer below
releases them. Post the same bytes again after any answer that says to.

## Answers

| Status | `code` | What it means | What the wallet does |
| --- | --- | --- | --- |
| `200` | | `{"transactions": ["0x…"]}`: submitted, not yet mined. Posting the same bytes again, before or after it mines, answers with the same hash and sends nothing new. | Watch the transaction, then settle from the chain. |
| `400` | `rejected` | Refused: nothing from this proof was sent, and nothing will be. | Drop the send and free its notes. |
| `400`, `415`, `422` | `invalidRequest` | The body is not JSON (`400`), not sent as `application/json` (`415`), or not this request (`422`): a field missing, mistyped, or not one of the four. Nothing was sent. | As `rejected`. |
| `413` | none, from the gateway | The body is over 132 KiB. Nothing was sent. | As `rejected`. |
| `404`, `405` | `notFound`, `methodNotAllowed` | This relayer has no such route. Nothing was sent. | As `rejected`. |
| `409` | `alreadySpent` | A note it spends is spent on chain, or a transaction the relayer already sent spends it, pending or mined: this proof or another of the same notes. `{"code": "alreadySpent", "error": "…", "transactions": ["0x…"]}` names the relayer's own transactions that spend them; it is empty when the notes went in one the relayer didn't send or no longer remembers. | Settle from the chain; don't free the notes. |
| `503` | `unavailable` | An earlier send of these notes has no known outcome yet, the relayer can't price gas right now, or the gateway is busy. | Post the same bytes again later. |
| `500` | `internal` | The relayer could not read the chain or record the send. Nothing is known. | Post the same bytes again later. |

A transport failure, a timeout or any other answer: post the same bytes again later. The relayer
records each transaction before broadcasting it, so a repeat never sends a second one; once the
first is known it answers `200` with its hash, or `409` naming it to another proof of the same
notes. A
recorded transaction no node knows is broadcast again after a minute if its nonce is still free,
and forgotten once its nonce went to another transaction mined twelve blocks deep, after which the
same bytes are checked and sent afresh. `200` means submitted: the transaction can still revert,
and the notes are spent only once their nullifiers are on chain. The same bytes keep naming a
send that reverted; to try again with notes it left unspent, prove again.

The `rejected` reasons, in the `error` field, for the wallet's logs:

- `this relayer sends no Railgun transactions`, `wrong chain`
- `wrong destination or ETH value`, `the calldata is too large`, `not a Railgun transact call`,
  `noncanonical transact calldata`
- `expected between one and sixteen Railgun transactions`, `each Railgun transaction must spend
  notes`, `a proof is for another chain`, `a proof is bound to an adapt contract`, `redirected
  unshields are not supported`
- `a proof's minimum gas price is above the relayer's cap`, `the transaction does not pay the
  relayer's fee` (below `fee`, or, where it prices gas, below what the gas costs at the honored
  rate: fetch the terms again and prove again)
- `the network's gas price is above the relayer's cap`, `the transaction needs more gas than the
  relayer's cap`, `the relayer is low on ETH`, `the transaction fails in simulation`

Errors never carry calldata, ciphertexts or the chain node's own messages.

## How the relayer checks a send

Before sending anything it decodes the calldata as Railgun's V2.1 `transact(Transaction[])`
(`abi/V2.1/RailgunSmartWallet.json`) and requires its canonical encoding, the proxy as `to`, no
ETH, and each Railgun transaction to spend notes, on this chain, with a zero adapt contract and
adapt parameters, and an unshield that is none or normal. It reads its fee the way the SDK's
`extractFirstNoteERC20AmountMapFromTransactionRequest` reads one for public broadcasters: the
first output of each Railgun transaction, decrypted with the relayer's viewing key, counts if its
note is the commitment the transaction adds and is in the relayer's token; the sum must reach
`fee`. The proof binds those ciphertexts, so nobody can swap the note. It then simulates the call
on the pending state at its gas price, which runs Railgun's own checks of the proofs, their roots,
nullifiers and minimum gas prices, and estimates its gas. Where it prices gas, the fee must also
cover that gas at the price it pays, at the lowest rate it quoted in the last ten minutes (so a
proof made from terms fetched then still goes, though ETH rose since): fee × 10^18 ≥ gas × gas
price × rate. The SDK's fee pays for 120% of its own estimate, which leaves room for the two
estimates and a small rise in the gas price to differ. It sends within its gas caps, with no
retries.

## Screening

Where Railgun screens (POI), as on Sepolia and Ethereum, a send's outputs, the change, the
recipient's note and the relayer's fee, are spendable only once screening proofs of the send
reach Railgun's screening nodes. The SDK builds such proofs before a send
(`preTransactionPOIsPerTxidLeafPerList`, which public broadcasters receive with the
transaction); this relayer neither takes, checks nor forwards them. Otherwise the sender's wallet
proves its landed send itself after a balance scan, once its tree of Railgun transaction IDs
holds the send (`generatePOIsForWallet` runs this on demand).

On Sepolia that second way is closed as of 2026-10-08: Railgun's indexer breaks its chain of
transaction verification hashes at index 4188 (block 11816741), so the SDK stops syncing
transaction IDs there and never proves a later send. Live, a relayed send's change stayed
`MissingExternalPOI` for 40 minutes while the screening nodes had validated the send. Until
Railgun mends the indexer, or the relayer forwards the pre-send proofs, a relayed send's outputs
on Sepolia stay unspendable; spend other notes meanwhile. On Ethereum the indexer's chain is
whole (140,430 transactions on 2026-10-08) and the screening node keeps up, so nothing blocks the
wallet's own proofs there; a relayed send's outputs clear once the wallet has proved it.

## Running a relayer that sends them

```toml
providers = ["coinmarketcap", "alchemy"]   # top level: live ETH and USDC prices, every fee by gas
fee_margin_bps = 1000      # a fee's margin over what its gas costs

[railgun_sends]
fee = 500000               # base units of the relayer's token every send pays at least
max_gas_limit = 3000000
max_gas_price_wei = 50000000000
journal = "/var/lib/zecswap-relayer/railgun-sends.sqlite"
```

`RELAYER_RAILGUN_SEED` holds the 64-byte BIP-39 seed of the relayer's own Railgun wallet, in hex,
beside `RELAYER_PRIVATE_KEY`: never the maker's, and never logged. `providers`, at the top of
the config because the relayer's swap fees follow gas the same way, takes the maker's keys,
`ZCASH_CMC_KEY` and `ALCHEMY_API_KEY`, and asks them in order as the maker does; without
`providers` a send pays `fee` alone. `ALCHEMY_API_KEY` also values what each send cost and
earned (below), with or without `providers`. Keep its mnemonic offline: the
fees collect there. The relayer refuses to start without it, or when it is the maker's account.
The journal records every send before its broadcast, which is what keeps a repeat from sending
twice; keep it with the relayer's state, never delete it while sends are pending, and run one
relayer per key and journal. A one-note transfer with fee and change burns about 1.07M gas on a
fork of Sepolia and 1.34M on Sepolia itself, and the relayer's account must hold that at its gas
price to send it.

## What each send earns and costs

The relayer pays each send's gas and is paid its fee note, so every send is a small trade of ETH
for the relayer's token. Its journal keeps what each came to, beside the send (`send_costs`):
once a send mines, a background pass reads its receipt (whether it succeeded, the gas it used and
the effective gas price) and reads from the send's own journaled bytes whether it was a private
send or an unshield and the fee its notes pay the relayer. A send that reverts burns its gas and
pays nothing. With `ALCHEMY_API_KEY` set, the pass values the gas in ETH and the fee in USDC at
the five-minute candle around the send's block; without it they are recorded unvalued. Sends
journaled before this ledger existed are read the same way.

`GET /relayer/v1/monitor/sends?since=<unix seconds>` (with `RELAYER_MONITOR_TOKEN`) lists them,
newest first, at most 1,000: `kind` (`send` or `unshield`), `fee` (token base units), `succeeded`,
`gasUsed`, `gasPriceWei`, `blockTime`, `ethUsd` and `tokenUsd`, all missing until the send mines and
is read; and the current `fee` and gas caps. A relayer that sends no Railgun transactions answers
`404`. zapp-dashboard's `/bridge/profit` charts what each send made and the fee that would cover
today's gas.

A one-note private send burned 1,069,971 gas and a withdrawal 1,120,796 on a fork of Sepolia
(2026-10-08). With ETH at $2,481 that was about $0.32 at that day's 0.12 gwei on Ethereum, and
$2.65 at 1 gwei: a fixed fee can't follow gas, which is why the relayer prices sends by it. `fee`
is the floor, for when gas is cheap; the dashboard shows what each send made.

To move to Ethereum, point `evm_rpc`, `contract` (a deployment whose `RAILGUN()` is Railgun's
proxy there, `0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9`) and `token` at mainnet and set the
caps for its gas: nothing in the sending is Sepolia's. Without `providers` the fee is fixed in
token units, and `fee` must cover `max_gas_limit` × `max_gas_price_wei` in ETH at its price;
with them it follows gas. Wallets pin the new `chainId` and `railgunProxy` from the terms.
