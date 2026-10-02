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
6. The maker takes a claim lock and reveals its share, crediting USDC to its inventory.
   The app reads and verifies that share, combines it with its own, and sweeps ZEC home.
   `depositZat` is the gross joint-account payment; the receive sweep has its own Zcash fee.

The contract retains its original storage roles: `Swap.maker` is the USDC side and
`Swap.user` is the ZEC side. For reverse swaps these are the user's auth address and the
maker's address respectively; the stored shares are likewise reversed. The reverse quote
keeps the ordinary business meanings of maker and user. Its ID is
`keccak256(abi.encode(userAuthAddress, makerShare))`.

## Waiting, cancellation, and recovery

`GET /v1/reverse/swaps/{swapId}` returns typed progress and the recorded ZEC transaction.
Suggested app steps are “Sending private USDC”, “Confirming escrow”, “Receiving ZEC”,
“Confirm ZEC received”, and “Moving ZEC to your wallet”. Show a refund action when funding
or readiness cannot finish. A submitted transaction is not a completed swap.

Before ready, the user can cancel. After ready, refund becomes available at `refundAfter`,
subject to the existing alternating claim/refund locks. Ready expires at `readyDeadline`.
Unlike the forward route, an unready reverse escrow **never becomes claimable just because
time passed**. An offline phone therefore cannot lose its USDC to a timeout claim. Funds can
remain locked until the phone resumes; the maker cannot sign a cancellation for the user.

The relayer exposes `/v1/reverse/lock-refund`, `/refund`, `/refund-payout`, and `/rescue`.
The app must observe a refund lock with the reveal margin remaining before sending its secret.
Refund settlement reveals the user's share without making token calls; the maker can recover
its ZEC. A separate payout shields USDC only to the committed refund note, less the signed fee.
Failed shielding can be retried, and returned notes use the existing per-swap vault rescue.
Ready and lock submissions currently use sponsored relayer gas; refund payout uses its configured
fee. Initial funding can also use the opt-in Sepolia sponsorship below. It adds no
success-path fee reimbursement.

## Sponsored initial funding (Sepolia)

The relayer accepts `POST /v1/reverse/fund` when its `[reverse_funding]` configuration
is present. `GET /v1/terms` then includes `reverseFunding` with `relayAdapt`, `token`,
`maker`, `maxGasLimit`, `maxGasPriceWei` (decimal string), and `maxCalldataBytes`.
The ordinary `fee` in terms still applies to payouts, not this sponsored transaction.
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
   amount, open the signed escrow, then adapter `shield` with one ERC20 request of value
   zero (shield all remaining dust). Budget Railgun's unshield fee so the adapter receives
   at least the full escrow amount. The user's proof binds every call and the dust note.
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
the relayer account against pending state before submission, including on-chain Railgun
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

Enable the example relayer configuration with a verified adapter and fund its existing
`RELAYER_PRIVATE_KEY` account with Sepolia ETH. Startup rejects other chains, the maker's
own gas account, and adapters whose `railgun()` differs from settlement's `RAILGUN()`.
This code does not enable sponsorship on the hosted service automatically. Mainnet
Railgun broadcaster submission and private fee-token reimbursement remain separate work,
including verification of escrow failure recovery on that submission path.

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
refund reveal. This closes preparation delays, but timely inclusion after sending the secret
still depends on the relayer, chain availability, and the lock window.
