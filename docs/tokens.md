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
   key; `GET {issuer}/v1/challenge`, then `POST {issuer}/v1/tokens` with the blinded messages
   and the install's attestation of them ([Attestation](#attestation)); then each blind
   signature unblinded and checked. The issuer signs only as many as the device's
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
- `GET /v1/challenge` → `{challenge}`: 32 random bytes, base64url, for the next
  `POST /v1/tokens` to sign. Each is good once, for five minutes; while too many are outstanding
  the issuer answers `503`.
- `POST /v1/tokens` with `{attestation: {challenge, chain, signature}, blinded: [...]}`
  (base64url; 1 to 100 blinded messages of 256 bytes) → `{blindSignatures: [...]}` for the first
  of them in order, as many as the device's allowance for the UTC day has left. It answers `400`
  for malformed blinded messages, `403` if the attestation is refused, and `429` once none is
  left. The challenge is spent by any request that gets as far as the attestation, refused or
  not; a refused one costs the device none of its allowance. The issuer never sees a token
  challenge, so it signs tokens for whatever day the app blinded them for; the app fetches for
  the day the maker asks.

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
- The issuer still learns which installs fetch tokens and how many: it sees the install's key
  on every fetch, as any limit per install must. Run it apart from the maker, ideally by another
  party, and keep no request logs; it keeps only today's counts, by the key's digest, and logs no
  chain, key, challenge or device id. The maker keeps each swap's request for its token back
  with the swap, and logs neither tokens nor requests.

## Attestation

The issuer turns a device's attestation into a stable id per install, whose tokens it counts per
day (`zecswap_issuer::Attester`). Two modes:

- **`android-key`**: Android hardware key attestation. Each install makes one key in the phone's
  secure hardware and keeps it; the hardware certifies the key in a chain up to Google's root,
  with a record of the app that made it and how the phone booted. The key signs every request,
  and the issuer counts tokens by the key's digest.
- **`insecure-test`**: takes the first certificate's place in the chain for the device's id,
  unchecked, so anyone can be any number of devices and it limits nothing. It exists so the path
  can be tested without phones, and the issuer refuses to start in it unless its config also
  says `allow_insecure = true`.

### What the app does

Once per install, it makes an EC P-256 key in the Android Keystore (`KeyGenParameterSpec`,
`PURPOSE_SIGN`, digest SHA-256, StrongBox where the phone has it) with the attestation challenge
`SHA-256("zecswap-issuer-v1" ‖ issuer)`, `issuer` being the name `GET /v1/token-key` gives,
UTF-8. Android attests a key only when it makes it, so the challenge names the issuer rather
than a request: a key made for anything else counts for nothing here. The app keeps the key
under one alias and reuses it for every fetch.

For each fetch, after `GET /v1/challenge`, it sends `POST /v1/tokens` with:

```json
{
  "attestation": {
    "challenge": "<the challenge, as given>",
    "chain": ["<the key's certificate chain from KeyStore.getCertificateChain, leaf first, each DER, base64url>"],
    "signature": "<SHA256withECDSA by the key, DER, base64url>"
  },
  "blinded": ["<blinded message, base64url>", "..."]
}
```

The signature covers `"zecswap-issuer-v1" ‖ challenge ‖ SHA-256(blinded₁ ‖ … ‖ blindedₙ)`: the
challenge's 32 bytes, and the blinded messages decoded and concatenated in the order sent. All
base64url is unpadded.

### What the issuer checks

In this order, every one before it signs anything, and a request failing any of them gets
`403` and touches no count:

1. The challenge is one it gave out, unused and under five minutes old; it is spent now,
   whatever follows.
2. The chain ends in one of the configured roots, Google's: a root that only comes with the
   chain counts for nothing, sent along or left out. Each certificate is signed by the next,
   whose subject it names, and each one above the leaf is marked a certificate authority and
   carries no attestation record of its own: an attested key can sign anything, a forged
   certificate too.
3. Every certificate, the root's included, is within its validity.
4. None is on Google's status list, a file the operator refreshes (`status_list`); the issuer
   reads it again whenever it changes.
5. The leaf's attestation record (extension 1.3.6.1.4.1.11129.2.1.17): its challenge is this
   issuer's; the attestation and the key both live in at least the configured security level
   (the trusted execution environment, or StrongBox); the secure hardware's root of trust says
   the bootloader is locked and the phone booted verified; the app is one of the configured
   packages, signed by one of the configured certificates.
6. The leaf key, P-256, signed the request.

### What it protects, and what it does not

It binds the daily allowance to an install of the genuine app on a phone with locked, verified
software: a script, an emulator, a phone with its bootloader unlocked (as rooting usually takes),
or another app gets nothing, and an install can't get more than its allowance by asking more
often or replaying another's requests.

It does not stop a person with a genuine phone from starting over: reinstalling the app, or
clearing its data, makes a new key and with it a fresh allowance, and so does the app's chain
expiring. Closing that takes something tied to the phone rather than the install, such as Play
Integrity's device recall, or a bond a device puts up. Nor does it hold against someone who
breaks into a locked phone's running system, who can then make keys as the app. The issuer, which sees each install's key,
must be run apart from the maker, which must never see an attestation.

## Running it

1. `zecswap-issuer keygen /etc/zecswap-issuer/key.pem` writes the issuer's signing key (mode
   0600) and prints its public half.
2. Save Google's attestation roots and status list where the config says, and refresh the status
   list at least daily (atomically: write a new file and rename it over the old). Set the app's
   package and signing certificate digest.
3. Configure and run `zecswap-issuer serve --config issuer.toml`
   (`crates/zecswap-issuer/issuer.example.toml`).
4. `zecswap-issuer keygen /etc/zecswap-maker/return-key.pem` writes the maker's own return key:
   a second key, never the issuer's. Its public half, which `GET /v1/info` also shows, is the
   one the app pins.
5. Give the maker a `[tokens]` table: the issuer's name, its origin (`maker`), the issuer's
   public key under `keys`, the return key's file as `return_key`, and a file for spent tokens,
   which keeps only today's. Turn it on only once the app spends tokens: until then every
   accept would be refused.
6. Rotating the issuer's key: generate a new one, list it first in the maker's `keys` with the
   old one after it, then switch the issuer to it. Tokens live a day, so the old key can go
   the day after the switch. The return key is pinned in the app: changing it takes an app
   release, and tokens handed back under the old one die at the end of their day.
