# Device limits without device identity

Accepting a quote costs the maker gas and ties up its inventory, while asking costs the requester
nothing. So each accept spends a Privacy Pass token, and the maker hands it back once the user
pays into the swap: a device may make as many swaps as it pays into, but walk away from only as
many a day as it has tokens (`tokens_per_day`, three to start with, reset at 00:00 UTC). An
issuer that checks the device is a genuine install gives it the day's tokens, signed blind: it
never sees a token, so it can't recognise one when it is spent, and the maker, which checks
tokens with public keys alone, never learns which device sent a request. The maker signs the
tokens it hands back blind too, so nothing ties one swap to another. Quotes stay free, and the
relayer takes no tokens: every swap it serves paid for its accept.

Everything here follows the standards: RFC 9577 for the HTTP exchange and RFC 9578 for the
publicly verifiable token type (`0x0002`: RSA blind signatures, RSABSSA-SHA384-PSS-Deterministic,
2048-bit keys). Any implementation of them interoperates; `zecswap-tokens` verifies RFC 9578's
own test vector. The Rust reference client (`zecswap_client::Tokens`) shows the whole flow.

## The flow

1. The app sends an accept without a token.
2. The maker answers `401` with `code: "tokenRequired"` and
   `WWW-Authenticate: PrivateToken challenge="<base64url TokenChallenge>", token-key="<base64url SPKI>"`.
   The challenge names the issuer, the service (`origin_info`, `maker`) and the UTC day: its
   32-byte redemption context is the number of days since the Unix epoch (Unix time divided by
   86 400, rounded down) as a big-endian `u64` in its last 8 bytes, zeros before. Every device
   gets the same challenge that day.
3. The app takes an unspent token for that challenge, its issuer's before any handed back, or
   fetches a batch from the issuer: for each token, a fresh 32-byte nonce and
   `token_input = 0x0002 ‖ nonce ‖ SHA-256(challenge) ‖ SHA-256(SPKI)`, blinded under the token
   key; `POST {issuer}/v1/tokens` with the device's attestation and the blinded messages; then
   each blind signature unblinded and checked. The issuer signs only as many as the device's
   day has left, so a batch may come back short, and answers `429` once none is left.
4. The app sends the accept again with `Authorization: PrivateToken token="<base64url token>"`,
   the token being `token_input ‖ authenticator` (354 bytes), and asks for it back: the body's
   `tokenRequest` is a fresh blinded request, made the same way, for the same challenge but
   under the maker's return key (`GET /v1/info` → `tokenReturnKey`). The app keeps the
   request's blinding secret and `token_input` with the swap.
5. The maker holds the token while the accept runs (another request with it meanwhile gets
   `401`) and keeps it spent only once it takes the quote. An accept refused before that, for an
   unknown or expired quote, a proof that doesn't verify, a missing or malformed `tokenRequest`
   or a busy maker (`503`), leaves the token spendable: the app spends it on its next accept.
   An error after the quote was taken (a `500`, say) leaves it spent; the maker then answers
   the next accept that sends it with `401`, and the app drops it and pays with another. A
   `401` always means the token itself was refused: drop it.
6. Once the user has paid in, the maker signs the request, blind: for a forward swap when the
   full deposit is in its joint account as the maker's wallet sees it, confirmed or not (an
   underpaid swap is cancelled like one never paid into), and for a reverse swap when the
   escrow is funded on-chain. It also signs it when a forward swap's `open` never landed, once
   `t1` shows it never will. The swap's status then carries the blind signature as
   `tokenReturn`: `GET /v1/swaps/{id}` for a forward swap, `GET /v1/reverse/swaps/{id}` for a
   reverse one. The app finalizes it with the request's blinding secret into a token under
   the return key, which the maker takes like the issuer's. A swap walked away from hands
   nothing back.
7. A token is good only on its UTC day: the maker takes only today's, so at 00:00 UTC every
   unspent one dies, issued or handed back, and the device's new allowance starts. A token
   handed back after midnight for an accept made before it is for the earlier day: drop it.

With `[tokens]`, the maker's `POST /v1/quote/{id}/accept` and `POST /v1/reverse/quote/{id}/accept`
take a token and a `tokenRequest`; without it, they refuse a `tokenRequest`, since nothing
would come back. Quotes, reads and the relayer take none.

## The maker's API

`zecswap_api` has the wire types.

- `GET /v1/info` → `tokenReturnKey`: the key the maker hands tokens back under (base64url SPKI),
  or none if accepts take no tokens.
- The accept bodies (`Acceptance`) → `tokenRequest`: an RFC 9578 `blinded_msg`, base64url, 256
  bytes, under the return key.
- `GET /v1/swaps/{id}` → `{swapId, tokenReturn}` for a forward swap, and `tokenReturn` in the
  reverse `GET /v1/reverse/swaps/{id}`: the RFC 9578 `blind_sig`, base64url, once the swap
  hands its token back, else `null`. Serving it to anyone who asks is safe: only the blinding
  secret turns it into a token.

## The issuer's API

`zecswap_api::tokens` has the wire types.

- `GET /v1/token-key` → `{issuer, tokenKey, tokensPerDay}`: the issuer name challenges carry, the
  token key (base64url SPKI) and the daily allowance.
- `POST /v1/tokens` with `{attestation, blinded: [...]}` (base64url; 1 to 100 blinded messages of
  256 bytes) → `{blindSignatures: [...]}` for the first of them in order, as many as the device's
  allowance for the UTC day has left. It answers `429` once none is left, `403` if the
  attestation is refused. It never sees a challenge, so it signs tokens for whatever day the
  app blinded them for; the app fetches for the day the maker asks.

## What keeps it private

Blinding hides which token the issuer or the maker signed. What remains to protect is up to the
app:

- **Pin both keys** in the app build, the issuer's token key and the maker's return key, and
  refuse a challenge or a `tokenReturnKey` naming any other: an issuer or maker that gave some
  devices their own key would recognise their tokens. The reference client checks the
  challenge's key against the one the issuer publishes and asks for tokens back under the key
  it was built with.
- **Refuse any day but today.** A day only some devices were asked for marks their tokens just
  as a key would: refuse a challenge whose redemption context is not a day, or not the
  current one by the app's own clock (allow a few minutes either side of midnight; the
  reference client allows ten).
- **Fetch ahead, spend later.** Fetch the day's batch in the background, well before it is
  needed, and never right before a request: a fetch and a spend seconds apart, from the same
  network address, can be matched. Send both over Tor or Oblivious HTTP where possible.
- **Spend the issuer's tokens first**, and one handed back only once they run out. Never spend
  a returned token the moment it arrives: a return and a spend seconds apart tie the swap that
  returned it to the one it pays for. Collect returns lazily, along with the swap's other
  reads, and leave hours, or at least a random delay, before spending one.
- **A fresh token, never the spent one.** The maker hands back a new token, signed blind, not
  the one the accept spent, whose nonce would tie the swaps together. A token whose accept was
  refused is different: the maker never kept it, so spending it again ties the refused attempt,
  which made no swap, to the next accept, and nothing more.
- **Never send the attestation, or anything identifying, to the maker.** Only the issuer sees
  the attestation, and it sees nothing of the swaps.
- The issuer still learns which installs fetch tokens and how many. Run it apart from the maker,
  ideally by another party, and keep no request logs; it keeps only today's counts. The maker
  keeps each swap's request for its token back with the swap, and logs neither tokens nor
  requests.

## Attestation

The issuer turns a device's attestation into a stable id per install, whose tokens it counts per
day (`zecswap_issuer::Attester`). The Zapp identity (App Attest on iOS, Play Integrity on
Android) will provide that. Until then the only mode is `insecure-test`, which takes the
attestation for the device id unchecked: anyone can be any number of devices, so it limits
nothing. It exists so the whole path can be built and tested now; the maker's
`max_awaiting_deposit` is what bounds spam meanwhile.

## Running it

1. `zecswap-issuer keygen /etc/zecswap-issuer/key.pem` writes the issuer's signing key (mode
   0600) and prints its public half.
2. Configure and run `zecswap-issuer serve --config issuer.toml`
   (`crates/zecswap-issuer/issuer.example.toml`).
3. `zecswap-issuer keygen /etc/zecswap-maker/return-key.pem` writes the maker's own return key:
   a second key, never the issuer's. Its public half, which `GET /v1/info` also shows, is the
   one the app pins.
4. Give the maker a `[tokens]` table: the issuer's name, its origin (`maker`), the issuer's
   public key under `keys`, the return key's file as `return_key`, and a file for spent tokens,
   which keeps only today's. Turn it on only once the app spends tokens: until then every
   accept would be refused.
5. Rotating the issuer's key: generate a new one, list it first in the maker's `keys` with the
   old one after it, then switch the issuer to it. Tokens live a day, so the old key can go
   the day after the switch. The return key is pinned in the app: changing it takes an app
   release, and tokens handed back under the old one die at the end of their day.
