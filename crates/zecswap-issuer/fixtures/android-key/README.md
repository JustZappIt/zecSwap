# A real phone's attestation

`android::tests::a_real_phones_chain_passes` checks a request captured from a real phone against
Google's real roots. It is ignored until both are here; then run

```sh
cargo test -p zecswap-issuer -- --ignored a_real_phones_chain_passes
```

- `roots/*.pem`: Google's hardware attestation roots, as the issuer's `roots` lists them.
- `captured.json`: one `POST /v1/tokens` the app sent, with what the issuer was configured with
  and when:

```json
{
  "issuer": "the issuer's name, which the key's attestation challenge was made for",
  "package": "xyz.justzappit.zapp.testnet",
  "signingDigest": "SHA-256 of the app's signing certificate, hex",
  "at": 1791000000,
  "blinded": ["the request's blinded messages, base64url, as sent"],
  "attestation": {"challenge": "…", "chain": ["…"], "signature": "…"}
}
```

`at` is the Unix time the request was made: the test checks the chain's validity then, and treats
the challenge as given out just before. A captured request names an install's key, and so the
phone: keep it out of public places once the repository is public.
