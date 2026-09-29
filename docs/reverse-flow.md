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
fee. No success-path fee reimbursement is added by this change.

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
Relay Adapt proof/broadcast bridge. Those integrations must follow the ordering above. Local
tests cover signatures, escrow settlement, private refunds, persistence, and forward-flow
regressions; a real Railgun-to-Zcash reverse swap remains a live integration check.
