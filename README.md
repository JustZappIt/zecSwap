# ZecSwap

Non-custodial swaps of shielded ZEC for USDC on Base. The ZEC goes to an Ironwood address
whose spend key is split between the maker (`e`) and the user (`z`); a Base contract pays
out against whichever half is revealed, verifying it on Pallas. Nothing touches a
transparent address and nothing is custodied: every failure ends in a refund.

| Path | What it is |
|---|---|
| `crates/zecswap-core` | Split-key primitives: joint account, proofs of knowledge, seed-derived shares, PCZT signing with `±(e + z)` |
| `crates/zecswap-chain` | Zcash light wallet (sync, joint accounts, sweeps) and the Base contract client |
| `crates/zecswap-api` | The maker's quote API on the wire, shared by makers and wallets |
| `crates/zecswap-client` | The user side of a swap, step by step, as the wallet runs it |
| `crates/zecswap-maker` | Maker service: quote API, store, and the watchtower that settles every swap |
| `crates/zecswap-cli` | Test wallet and user CLI for testnets |
| `crates/zecswap-e2e` | Live end-to-end suite on Base Sepolia and the Zcash testnet |
| `contracts` | `ZecSwap.sol` (two-phase lock-then-reveal state machine) and `Pallas.sol` (Foundry) |

The reverse Railgun-USDC-to-ZEC service, authorization flow, and Android integration boundary
are described in [the reverse flow guide](docs/reverse-flow.md).

## Tests

```sh
cargo test --workspace                     # crypto and policy edge cases, plus local Ironwood spends with real proofs
(cd contracts && forge test)               # contract unit, fuzz and invariant tests
cargo build --release -p zecswap-core --example evm_vectors
(cd contracts && FOUNDRY_PROFILE=ffi forge test)   # Pallas.sol fuzzed against the Rust implementation
scripts/e2e-testnet.sh [scenario ...]      # live testnets, about 50 minutes
```

The live suite deploys fresh contracts, runs an attentive and a silent maker in-process and
plays six scenarios concurrently, checking both chains at the end of each: `happy`,
`no-deposit`, `underpaid`, `silent-maker`, `never-claimed` and `abandoned-claim`. It restarts
the attentive maker partway through. It needs a Base Sepolia key with test ETH and a
`zecswap-cli` wallet holding testnet ZEC; see the script for the variables.

```sh
cargo run -p zecswap-cli -- --data-dir .testnet/user init     # prints the address to fund
cargo run -p zecswap-cli -- --data-dir .testnet/user status
```

## Running a maker

```sh
cp crates/zecswap-maker/maker.example.toml maker.toml         # then fill in the addresses
MAKER_PRIVATE_KEY=... MAKER_ROOT_SECRET=... cargo run -p zecswap-maker -- --config maker.toml serve
```

Deploy the contract with `contracts/script/Deploy.s.sol`.
