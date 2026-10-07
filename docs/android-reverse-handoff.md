# Android reverse swap handoff

Implement Railgun shielded USDC → shielded ZEC in the Android app. The app currently supports
ZEC → Railgun USDC only. Preserve that flow and its recovery behavior. Follow the app's
AGENTS.md, architecture, strong typing, persistence, background-work and UI conventions.
Use minimal comments and ask before changing protocol assumptions.

The server, contract, Rust reference client and JNI/Kotlin library are in the sibling
`zecSwap` repository on `feature/android`. Read these before implementing:

- `docs/reverse-flow.md` and `docs/deployment.md`
- `deployments/sepolia-terms-hash.json`
- `crates/zecswap-api/src/reverse.rs` and `service.rs`
- `crates/zecswap-client/src/reverse.rs`
- `android/src/main/java/xyz/justzappit/atomicswap/ReverseAtomicSwap.kt`
- `contracts/src/ZecSwap.sol`

Use the deployed testnet maker at `https://zecswap-testnet.pepeman931.workers.dev/maker`
and relayer at `https://zecswap-testnet.pepeman931.workers.dev/relayer`. Pin Sepolia chain ID
11155111, contract `0xD75Efc6a157CC0A95f66962DA86DDf35d9F2617c`, and test token
`0x5764d0044bef5aa839e0ddafe2073421101b9ed8` from the deployment manifest. Verify `/v1/info`
against these pins. This contract (October 7) stores only a hash of each swap's terms: every
call on a swap passes the terms after its id, and a reverse swap's id is `reverseSwapId`
([reverse-flow.md](reverse-flow.md)). The manifest contains the other public deployment addresses. Never ship
server keys, seeds, tunnel credentials or private RPC credentials. Keep existing pending swaps
bound to their original deployment; do not silently migrate them to the new contract.

Implement this normal flow with two user authorizations:

1. Derive and persist a fresh swap index, auth key, Railgun refund-note commitment and verified
   quote. Accept it and import the joint Zcash account before funding.
2. First authorization: use the existing Railgun integration to build and prove
   one atomic Relay Adapt transaction that unshields the exact escrow amount, approves it,
   and calls `openReverse`. Discover initial funding sponsorship via the relayer's
   `GET /v1/terms` `reverseFunding` field and use `POST /v1/reverse/fund` with the persisted
   transaction as described in `docs/reverse-flow.md`. For sponsored Sepolia funding, use
   V2 legacy Relay Adapt, `sendWithPublicWallet=true`, no broadcaster fee recipient,
   `requireSuccess=true`, a `transfer` of the advertised `reverseFunding.fee` to the relayer
   after `openReverse`, and a final shield of all remaining escrow-token dust. The relayer
   pays Sepolia ETH and is reimbursed by that transfer; do not require or fund a phone EVM gas
   account. Absent sponsorship is an
   explicit unsupported state, not permission to switch submission methods automatically.
   Account separately for Railgun fees and the relayer's funding fee. Do not
   substitute an ordinary public ERC20 transfer for the escrow funding. The maker detects confirmed funding itself;
   there is no separate funded notification.
   A funding response contains a pending hash, not confirmation; an empty transaction list
   means matching escrow already exists. Independently verify the escrow in both cases.
3. Show resumable progress while independently verifying escrow and scanning the joint Zcash
   account. The maker's status and reported transaction ID are advisory. Only enable the
   second authorization when the full quoted `depositZat` is confirmed and spendable.
4. Second authorization: sign `Ready` through `ReverseAtomicSwap` and submit to the relayer.
   Verify the maker's on-chain claim and revealed share, combine the shares, and sweep ZEC
   into the user's shielded wallet. Show the net receive amount after the Zcash sweep fee.

Use typed DTOs and explicit states, integer base-unit amounts, decimal-string API amounts,
and validated fixed-length hex fields. Never use floating point for money or error text for
branching. Keep the existing native API conventions; the three packaged Android ABIs already
include reverse signing methods. Reverse roles in the escrow's terms differ from business
names: `Terms.maker` is the USDC user, and `Terms.user` is the ZEC maker. The contract stores
only their hash, so relayer requests carry them (`docs/reverse-flow.md`, "Terms hash").

Persist the quote, consumed index, imported account/birthday, funding transaction, authorized
pending actions and receive transaction before submission. Recover after process death,
network loss and device restart by reconciling chain state. Never unshield or pay again merely
because an HTTP request timed out. Keep the swap worker active using the app's established
Android background-work pattern.

Implement cancellation, refund locking, private refund payout and retry/rescue handling from
the reference client. Never reveal the user's share before verifying a valid refund lock with
enough inclusion time remaining. Before Ready the user can cancel; after Ready the refund
deadline and lock rules apply. An offline phone must not automatically authorize Ready.

Add meaningful tests for both directions, role mapping, malformed/mismatched quotes, partial
or unconfirmed ZEC deposits, duplicate callbacks, interrupted funding, restart recovery,
deadlines, cancellation and payout retries. Validate on a device with a real Railgun-funded
testnet swap. The hosted smoke tests covered public-test-token escrow funding, ZEC settlement,
restarts and private refunds; they did not exercise Railgun unshield-to-escrow funding.

Do not add gas budgets or the deferred API abuse-protection work. Do not reorder the existing
forward flow: its escrow must exist before the user's ZEC deposit to preserve recovery.
Report implementation changes, validation results and any remaining Railgun/device blockers.
