# Device limits without device identity

Accepting a quote costs the maker gas and ties up its inventory, while asking costs the requester
nothing. So each accept spends a Privacy Pass token: one token, one swap. A device gets a day's
tokens (`tokens_per_day`, three to start with, reset at 00:00 UTC) from an issuer that checks it
is a genuine install, but the issuer signs them blind: it never sees a token, so it can't
recognise one when it is spent, and the maker, which checks tokens with the issuer's public key
alone, never learns which device sent a request. Each device is limited, and no one can link its
swaps. Quotes stay free, and the relayer takes no tokens: every swap it serves paid for its
accept.

Everything here follows the standards: RFC 9577 for the HTTP exchange and RFC 9578 for the
publicly verifiable token type (`0x0002`: RSA blind signatures, RSABSSA-SHA384-PSS-Deterministic,
2048-bit keys). Any implementation of them interoperates; `zecswap-tokens` verifies RFC 9578's
own test vector. The Rust reference client (`zecswap_client::Tokens`) shows the whole flow.

## The flow

1. The app sends an accept without a token.
2. The maker answers `401` with `code: "tokenRequired"` and
   `WWW-Authenticate: PrivateToken challenge="<base64url TokenChallenge>", token-key="<base64url SPKI>"`.
   The challenge names the issuer and the service (`origin_info`, `maker`), with no redemption
   context, so tokens can be fetched long before they are spent.
3. The app takes an unspent token for that challenge, or fetches a batch from the issuer:
   for each token, a fresh 32-byte nonce and `token_input = 0x0002 ‖ nonce ‖ SHA-256(challenge) ‖
   SHA-256(SPKI)`, blinded under the token key; `POST {issuer}/v1/tokens` with the device's
   attestation and the blinded messages; then each blind signature unblinded and checked. The
   issuer signs only as many as the device's day has left, so a batch may come back short.
4. The app sends the accept again with `Authorization: PrivateToken token="<base64url token>"`,
   the token being `token_input ‖ authenticator` (354 bytes). A token is spent once, whether or
   not the accept then succeeds; discard it after sending.

With `[tokens]`, the maker's `POST /v1/quote/{id}/accept` and `POST /v1/reverse/quote/{id}/accept`
take a token. Quotes, reads and the relayer take none.

## The issuer's API

`zecswap_api::tokens` has the wire types.

- `GET /v1/token-key` → `{issuer, tokenKey, tokensPerDay}`: the issuer name challenges carry, the
  token key (base64url SPKI) and the daily allowance.
- `POST /v1/tokens` with `{attestation, blinded: [...]}` (base64url; 1 to 100 blinded messages of
  256 bytes) → `{blindSignatures: [...]}` for the first of them in order, as many as the device's
  allowance for the UTC day has left. It answers `429` once none is left, `403` if the
  attestation is refused.

## What keeps it private

Blinding hides which token the issuer signed. What remains to protect is up to the app:

- **Pin the token key** in the app build, and refuse a challenge naming any other: an issuer that
  gave some devices their own key would recognise their tokens. The reference client checks the
  challenge's key against the one the issuer publishes.
- **Fetch ahead, spend later.** Fetch a batch in the background, well before it is needed, and
  never right before a request: a fetch and a spend seconds apart, from the same network address,
  can be matched. Send both over Tor or Oblivious HTTP where possible.
- **Never send the attestation, or anything identifying, to the maker.** Only the issuer sees
  the attestation, and it sees nothing of the swaps.
- The issuer still learns which installs fetch tokens and how many. Run it apart from the maker,
  ideally by another party, and keep no request logs; it keeps only today's counts.

## Attestation

The issuer turns a device's attestation into a stable id per install, whose tokens it counts per
day (`zecswap_issuer::Attester`). The Zapp identity (App Attest on iOS, Play Integrity on
Android) will provide that. Until then the only mode is `insecure-test`, which takes the
attestation for the device id unchecked: anyone can be any number of devices, so it limits
nothing. It exists so the whole path can be built and tested now; the maker's
`max_awaiting_deposit` is what bounds spam meanwhile.

## Running it

1. `zecswap-issuer keygen /etc/zecswap-issuer/key.pem` writes the signing key (mode 0600) and prints
   its public half.
2. Configure and run `zecswap-issuer serve --config issuer.toml`
   (`crates/zecswap-issuer/issuer.example.toml`).
3. Give the maker a `[tokens]` table: the issuer's name, its origin (`maker`), the issuer's
   public key under `keys`, and a file for spent tokens. Turn it on only once the app spends
   tokens: until then every accept would be refused.
4. Rotating the key: generate a new one, list it first in the maker's `keys` with the old one
   after it, then switch the issuer to it. Once tokens under the old key have been spent or given
   up, drop it from `keys`; the maker then forgets its spent tokens.
