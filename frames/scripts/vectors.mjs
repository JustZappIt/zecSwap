// Frame transactions as @forshtat/hegota-support encodes them for the frames devnet, the known
// answers zecswap-chain's encoder is checked against (crates/zecswap-chain/tests/frames_vectors.rs):
//   node scripts/vectors.mjs > ../crates/zecswap-chain/tests/frame-vectors.json
import { privateKeyToAccount } from "viem/accounts";
import { profile, frame, newTx, secp256k1Placeholder, signTx, encodeTx, txHash, sigHash, toWire } from "./devnet.mjs";

// Throwaway keys of these vectors alone.
const KEYS = [
  "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
  "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
];
const bytes = (n, fill) => `0x${fill.repeat(n)}`;

const cases = [
  {
    name: "the sponsored claim's shape",
    key: KEYS[0],
    chainId: 1337,
    nonce: 0n,
    fees: { maxPriorityFeePerGas: 2_000_000_000n, maxFeePerGas: 2_000_000_000n },
    frames: [
      { mode: "VERIFY", scope: "EXECUTION", target: null, execution: 20_000n },
      { mode: "VERIFY", scope: "PAYMENT", target: "0x68cb7bf7010ce289bea39437482db0eaa342f91b", execution: 60_000n, data: bytes(388, "ab") },
      { mode: "SENDER", target: "0x09e06e55eb0600f05e7352a02620a95f420129f5", execution: 400_000n, state: 300_000n, data: bytes(420, "0e") },
      { mode: "SENDER", target: "0x09e06e55eb0600f05e7352a02620a95f420129f5", execution: 100_000n, state: 150_000n, data: bytes(100, "69") },
    ],
  },
  {
    name: "every field zero or empty",
    key: KEYS[1],
    chainId: 1337,
    nonce: 0n,
    fees: { maxPriorityFeePerGas: 0n, maxFeePerGas: 0n },
    frames: [{ mode: "DEFAULT", target: null, execution: 0n }],
  },
  {
    name: "a later nonce, value, and fees apart",
    key: KEYS[1],
    chainId: 1337,
    nonce: 300n,
    fees: { maxPriorityFeePerGas: 1n, maxFeePerGas: 7_000_000_007n },
    frames: [
      { mode: "VERIFY", scope: "EXECUTION_AND_PAYMENT", target: null, execution: 30_000n },
      { mode: "SENDER", target: "0x00000000000000000000000000000000000000aa", execution: 50_000n, state: 200_000n, value: 1_000_000_000_000_000n, data: "0x01" },
    ],
  },
];

const vectors = [];
for (const c of cases) {
  const sender = privateKeyToAccount(c.key).address.toLowerCase();
  const unsigned = newTx({
    chainId: c.chainId,
    sender,
    nonceSeq: c.nonce,
    fees: c.fees,
    signatures: [secp256k1Placeholder(profile)],
    frames: c.frames.map((f) => frame(profile, f.mode, { scope: f.scope, target: f.target, execution: f.execution, state: f.state, value: f.value, data: f.data })),
  });
  const tx = await signTx(profile, unsigned, c.key);
  const hex = (n) => `0x${n.toString(16)}`;
  vectors.push({
    name: c.name,
    chainId: c.chainId,
    nonce: hex(c.nonce),
    sender,
    maxPriorityFeePerGas: hex(c.fees.maxPriorityFeePerGas),
    maxFeePerGas: hex(c.fees.maxFeePerGas),
    frames: unsigned.frames.map((f) => ({ mode: f.mode, flags: f.flags, target: f.target, execution: hex(f.limits.execution), state: hex(f.limits.state), value: hex(f.value), data: f.data })),
    sigHash: sigHash(profile, unsigned),
    signature: tx.signatures[0].signature,
    raw: encodeTx(profile, tx),
    hash: txHash(profile, tx),
    wire: toWire(tx),
  });
}
console.log(JSON.stringify({ profile: profile.id, vectors }, null, 2));
