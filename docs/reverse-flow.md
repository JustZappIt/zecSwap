# Railgun USDC → shielded ZEC

The reverse route has two normal user authorizations: fund escrow, then approve settlement
after independently confirming the ZEC deposit. The maker's status API is advisory. Neither
a funding notification nor a reported ZEC transaction is proof that the other side paid.

## Protocol

1. Derive a fresh swap index and its auth key, user share, and Railgun refund note. Request
   `POST /v1/reverse/quote` with `units`, `user`, and `refundNote`. Persist the index and quote.
2. Verify the deployment, token, amounts, deadlines, maker proof, and refund commitment.
   Send the usual share proof and viewing keys to
   `POST /v1/reverse/quote/{quoteId}/accept`. Repeating the same acceptance returns the same
   swap. The maker reserves ZEC inventory and starts watching the joint account.
3. Import the joint viewing key locally **before funding**, saving its account and birthday.
   The first authorization signs `OpenReverse`. In one Railgun cross-contract transaction,
   unshield the escrow amount to Relay Adapt, approve the exact amount, and call
   `openReverse`. All calls must succeed together. Budget the Railgun unshield and broadcaster
   fees separately from escrow. This follows Railgun's
   [cross-contract transaction API](https://docs.railgun.org/developer-guide/wallet/transactions/cross-contract-calls).
4. The maker polls the accepted escrow automatically. No separate “funded” request is needed.
   It verifies every term and waits the configured EVM confirmations before preparing ZEC.
   It records the payment transaction before broadcasting and retries the same transaction.
5. The app scans the joint account and waits until the full quoted `depositZat` is spendable.
   The second authorization signs `Ready` and submits it to `POST /v1/reverse/ready` on the
   independent relayer. Do not offer this action based only on maker progress.
6. Once the ready is in a finalized block, the maker claims before `t1`, revealing its share
   and crediting USDC to its inventory.
   The app reads and verifies that share, combines it with its own, and sweeps ZEC home, to
   a never-used address: the maker holds the joint viewing key.
   `depositZat` is the gross joint-account payment; the receive sweep has its own Zcash fee.

The escrow's terms keep the contract's original roles: `Terms.maker` is the USDC side and
`Terms.user` is the ZEC side. For reverse swaps these are the user's auth address and the
maker's address respectively; the shares are likewise reversed, and `payoutNote` is zero.
`Quote::open(userShare).terms()` builds them. The reverse quote keeps the ordinary business
meanings of maker and user. Its ID is `reverseSwapId`,
`keccak256(abi.encode(userAuthAddress, makerShare, true))`, which is never a forward swap's
`keccak256(abi.encode(maker, userShare))`: a maker can't open the forward swap a user verified
as a reverse escrow, where that user would have no claim after `t0`. The contract stores only
`hashTerms(terms)`, so every relayer request on the escrow carries `terms` (see
[Terms hash](#terms-hash-2026-10-06)).

## Waiting, cancellation, and recovery

`GET /v1/reverse/swaps/{swapId}` returns typed progress and the recorded ZEC transaction.
Suggested app steps are “Sending private USDC”, “Confirming escrow”, “Receiving ZEC”,
“Confirm ZEC received”, and “Moving ZEC to your wallet”. Show a refund action when funding
or readiness cannot finish. A submitted transaction is not a completed swap.

Before ready, the user can refund at any time. Once ready, the escrow is the maker's to claim
until `t1` (`refundAfter`), and the user's to refund from `t1` on, for good. The contract takes a
ready only before `t0` (`readyDeadline`). Unlike the forward route, an unready reverse escrow
**never becomes claimable just because time passed**. An offline phone therefore cannot lose its
USDC to a timeout claim. Funds can stay in escrow until the phone resumes; the maker cannot sign
a cancellation for the user.

The relayer exposes `/v1/reverse/refund`, `/refund-payout`, and `/rescue`. The app sends its
share only in a refund whose window, read with the newest block, leaves the reveal margin. It
saves the deadline of every ready it signs before handing the signature out, and refunds an
unready escrow only once the chain is past that deadline or `t0`: a ready landing first would
revert the refund with the share public, the maker's to claim with. It saves that it asked for a
refund before sending one, and never signs a ready after it. The relayer likewise holds a refund
back, `503`, while a ready it sent may still land.
Refund settlement reveals the user's share without making token calls; the maker can recover
its ZEC. A separate payout shields USDC only to the committed refund note, less the signed fee.
Failed shielding can be retried, and returned notes use the existing per-swap vault rescue.
Ready submissions currently use sponsored relayer gas; refund payout uses its configured fee. Initial funding can also use the opt-in sponsorship below, which the funding transaction
itself pays for with a relayer fee in the escrow token.

## Sponsored initial funding

The relayer accepts `POST /v1/reverse/fund` when its `[reverse_funding]` configuration
is present. `GET /v1/terms` then includes `reverseFunding` with `relayAdapt`, `token`,
`maker`, `maxGasLimit`, `maxGasPriceWei` (decimal string), `maxCalldataBytes`, and `fee`
(decimal string, escrow-token base units). That `fee` is what each funding transaction pays the
relayer for its gas; the ordinary top-level `fee` in terms still applies to payouts.
An absent `reverseFunding` means sponsorship is disabled; do not silently switch to a
phone-funded transaction. Pin the chain, settlement, maker, token, and adapter before proving.

The phone keeps its Railgun spending keys and generates the proof locally:

1. Prepare the signed `approve` and `openReverse` calls as above. Use the **V2 legacy
   Relay Adapt** transaction format, not V3 or the EIP-7702 adapter.
2. Use `sendWithPublicWallet=true` and no broadcaster fee recipient throughout the Railgun
   estimate/prove/populate sequence. Here the public submitting wallet belongs to the
   relayer; the phone needs no ETH. This mode must produce `requireSuccess=true`.
   Paid broadcaster mode can allow failed cross-contract calls to continue and is rejected.
3. Include the escrow token in `relayAdaptShieldERC20Recipients`, addressed to the user's
   private wallet. The complete action must contain exactly: approve the exact escrow
   amount, open the signed escrow, `transfer` at least the advertised funding `fee` of the
   escrow token to the relayer's address (`relayer` in terms), then adapter `shield` with one
   ERC20 request of value zero (shield all remaining dust). Budget Railgun's unshield fee so
   the adapter receives at least the escrow amount plus the funding fee. The user's proof
   binds every call and the dust note. With `requireSuccess=true` the relayer is paid only
   if the escrow opens; its pre-submission simulation is what protects its gas.
4. Persist the prepared calldata, deployment, and swap ID, then send:

   ```json
   {
     "swapId": "0x<32-byte swap ID>",
     "chainId": 11155111,
     "to": "0x<V2 Relay Adapt address>",
     "data": "0x<complete proved relay calldata>",
     "value": "0"
   }
   ```

The endpoint accepts no spending key, sender, nonce, or caller-selected gas settings.
It validates the full call tree, EIP-712 authorization, proof-to-action binding, token,
maker, chain, zero ETH value, and deadline. It simulates the complete transaction from
the relayer's funding account against pending state before submission, including on-chain Railgun
proof checks, and caps the gas limit and gas price. It submits the original calldata
without modifying any proof-bound field. Calldata is limited to 64 KiB and the HTTP
body to 132 KiB; the nginx route has the matching limit.

The response is `{ "transactions": ["0x<transaction hash>"] }` as soon as submission
is acknowledged, **before confirmation**. Persist the hash and independently verify escrow
with the required confirmations. A matching escrow already on-chain returns an empty list
without another submission, including after a relayer restart. A different escrow is rejected.
After any timeout or error, reconcile first and only retry the identical prepared proof;
never generate another unshield merely because the response was lost. There is no durable
relayer submission journal: an ambiguous send can require chain reconciliation and a retry
can consume additional relayer gas. Railgun nullifiers and the unique escrow ID prevent
funding twice. The client must retain its pending action and must not treat an error as proof
that no transaction was submitted.

Enable it in the relayer's configuration and fund its `RELAYER_PRIVATE_KEY` account with ETH,
which tops its funding account up. Fundings go through Railgun's own Relay Adapt on the chain of
the relayer's `network`. Startup rejects a chain, Railgun or token of another network, a
`relay_adapt` other than Railgun's own there, the maker's own gas account, and adapters whose
`railgun()` differs from settlement's `RAILGUN()`. There is no aggregate sponsorship budget.
Operators opt in through configuration; the hosted Sepolia service is enabled as recorded
in [deployment.md](deployment.md#october-2-initial-funding-sponsorship). Mainnet
Railgun broadcaster submission (a private fee note with `requireSuccess=false`) remains
separate work, including verification of escrow failure recovery on that submission path.

Local tests cover a fixture encoded by the installed Railgun SDK, rejected transaction
mutations, API limits, and Anvil submission with the real escrow and a **mock** adapter.
The mock substitutes minting for Railgun unshield/proof verification. A real locally proved
Railgun-to-escrow transaction and Android device flow remain live integration checks.

Keep the app's foreground swap worker active through local deposit verification and settlement.
Persist before submitting: swap index and quote, joint account/birthday, funding transaction,
pending authorized action, and receive transaction. On restart, reconcile chain state first;
never unshield a second time because an HTTP request timed out. A delayed ready signature cannot
undo a cancellation. A sweep resumes from the recorded transaction or is rebuilt only after
expiry is established.

## Service setup

See [deployment.md](deployment.md) for public endpoints through Cloudflare and the persistent
host requirement.

Use a newly deployed contract with `openReverse`; existing deployed bytecode does not gain
these methods. Leave `[reverse]` absent to keep the maker's reverse admission disabled.
The example config includes optional settings. Supply `MAKER_ZCASH_SEED`, run `zec-inventory`
to obtain its shielded address, fund it, and run the existing `serve` command. Reopening an
inventory wallet with a different seed is rejected. Keep the seed, maker root, and databases.

Inventory is reserved at acceptance, including a configured Zcash fee reserve. Persisted
records retain payment and recovery transactions across restarts. A replacement is prepared
only after a fresh scan proves the previous transaction expired. Terminal EVM state waits
the configured confirmations before the maker forgets the joint account.

## Integration boundary

`GET /v1/info` reports the maker's chain, contract, token, Zcash network and whether reverse
admission is enabled. Compare these with the app's pinned deployment before quoting; this
metadata does not establish trust in a deployment. `GET /healthz` returns 204 only after a
recent completed watchtower pass, otherwise 503. It does not guarantee available inventory
or healthy upstream chains. Reverse startup rejects contracts without the reverse interface.

Both services return errors as `{ "code": "unknownSwap", "error": "swap is unknown" }`.
`code` is the shared `service::ErrorCode` enum; `error` is display text, never a branching key.
Malformed JSON, fields and path parameters use `invalidRequest`; missing routes and wrong
methods also return JSON. Responses carry `Cache-Control: no-store`. Reconcile chain state
after transport failures; an error does not establish that a transaction was never broadcast.

`zecswap-client::reverse::ReverseUser` provides the reference flow. The Android library exposes
`ReverseAtomicSwap` for opening, ready/refund authorizations, and the receive sweep, while
reusing `AtomicSwap.accept`, `payoutNote`, and `depositAccount`. Its native binaries include
the new methods.

This change does not wire the sibling Android app's screens, foreground worker, or Railgun
Relay Adapt proof bridge. Its submission can now use `RelayerApi::fund_reverse` and the
endpoint above. Those integrations must follow the ordering above. Local
tests cover signatures, escrow settlement, private refunds, persistence, and forward-flow
regressions; a real Railgun-to-Zcash reverse swap remains a live integration check.

## Authorization hardening (2026-10-02)

Rescue uses `Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)`.
Read `rescueNonces(id)` before approval. The contract checks the deadline and consumes the nonce
before calling the vault; a failed shield rolls consumption back. Both relayer rescue endpoints
require `nonce` and `deadline` in their JSON payloads. This changes the rescue ABI and typed data:
ship the contract, relayer, native library and wallet together for a new deployment. Existing
contracts cannot acquire this protection through an app update, and old signatures remain valid
there. Updated Android clients refuse new rescue approvals for contracts without the nonce getter.
Ordinary settlement and refunds for existing swaps remain supported. Do not move an existing
swap record to a different contract or represent old approvals as revoked.

Joint sweeps now require `SweepIntent` at the Rust and Kotlin signing boundaries. The caller
selects the destination independently of the PCZT builder and supplies a minimum receipt and
maximum fee. The signer verifies note/value commitments and encrypted outputs, rejects change
to another receiver, rejects transparent/Sapling components and preauthorized nonzero spends,
and signs only the matching joint inputs. Preserve note values and randomness until this check;
Android signs the unredacted local PCZT before proving. Ship the rebuilt JNI libraries with the
matching Kotlin wrapper. Android reverse receives retain the reviewed sweep fee; forward recovery
and the maker use a fixed 320,000-zatoshi ceiling.

Android rereads the matching confirmed escrow and current clock immediately before a reverse
refund reveal. This closes preparation delays. The relayer reads the refund's window again once
its turn to broadcast comes and refuses it, `400` with nothing sent, if less than its margin is
left; on mainnet it sends the refund through Flashbots Protect, which lands it only if it
succeeds, so one that would land reverted is dropped, its share never public
([deployment.md](deployment.md#switching-to-mainnet)). A user's refund has no end, an unready
escrow's from funding and a ready one's from `t1`: only a ready landing first can revert it, and
neither the app nor the relayer refunds while a ready it sent may still land.

## Terms hash (2026-10-06)

The contract keeps only `keccak256(abi.encode(terms))` of each swap's `Terms` (maker, token,
amount, makerKey, userKey, user, t0, t1, payoutNote) and reverts `WrongTerms` on any call whose
terms hash otherwise. `getSwap` returns the hash, stage, `paidOut` and the revealed
secret; `Opened` still emits the terms. Every call that acts on a swap takes `terms` after the
id, and every relayer request on a swap carries them as a `terms` object: `/v1/reverse/ready`,
`/refund` (and its nested payout), `/refund-payout` and `/rescue`. The relayer
reads the escrow against them before sending and rejects a mismatch. `/v1/reverse/fund` is
unchanged: `openReverse` takes the same `ReverseOpen` and stores the hash. Read an escrow by
comparing `getSwap(id).termsHash` with the hash of the terms the quote and the user's share
give; a mismatch means it is not the quoted escrow. This changes the ABI: ship the contract,
maker, relayer, native library and wallet together for a new deployment.

## Windows (2026-10-20)

The contract has no locks. At every moment at most one side may reveal, by the clock and the
stage alone: on a reverse escrow, the maker's claim from ready until `t1`, the user's refund while
it isn't ready and from `t1`. `ready` is taken only before `t0`, and `openReverse` refuses a `t0`
under `MIN_WINDOW()` from when it lands, or a `t1` under it from `t0`; the client checks a quote
leaves both from its funding deadline. `getSwap` returns four words (hash, stage, `paidOut`,
secret), and `/v1/reverse/lock-refund` is gone. Read `MIN_WINDOW()` and `LATE_WINDOW()` to tell
the contract apart: the lock contract has `LOCK_DURATION()` instead, and its record decodes
silently under the windows ABI, so a client that skips the check would refund there without the
lock it needs. A swap on the lock contract ends under its rules: a deployment switches contracts
only once none is live there. This changes the ABI: ship the contract, maker, relayer, native
library and wallet together.
