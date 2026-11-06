// The EF frames devnet (chain 1337) and what the probe and vectors share: its endpoints, keys from
// .env.frames, legacy sends, contract artifacts and the claim's frame transaction.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { encodeDeployData, encodeFunctionData, keccak256, encodeAbiParameters } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { profile } from "@forshtat/hegota-support/profiles/ethrex-eip8141-tracer";
import { frame, newTx, secp256k1Placeholder, signTx, encodeTx, txHash, sigHash } from "@forshtat/hegota-support/tx";
import { toWire } from "@forshtat/hegota-support/rpc";

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
export const CHAIN_ID = 1337;
export const ETHREX = process.env.FRAMES_ETHREX_URL ?? "https://altmempool.forshtat.com";
// The alt mempool node frame transactions go to: the miner's own. The gateway node
// (https://altmempoolgateway.forshtat.com/rpc) had no peers on 2026-11-06 and held what it admitted
// without ever passing it on.
export const MEMPOOL = process.env.FRAMES_MEMPOOL_URL ?? "https://altmempool.forshtat.com/rpc";
/** A transaction's page; pick the explorer's "Miner" instance, whose history holds it. */
export const explorerLink = (hash) => `https://explorer.altmempool.wtf/tx/${hash}`;
export const REGISTRY = "0xd51c04d7a0b01124e84de152f4e56d9eecb22d2f";
export const ENTRY_POINT = "0x00000000000000000000000000000000000000aa";
/** Legacy transactions: the base fee is 7 wei. */
export const LEGACY_GAS_PRICE = 2_000_000n;
/** Frame transactions: what the workshop pays. */
export const FRAME_MAX_FEE = 2_000_000_000n;
/**
 * The claim's fixed limits, from the probe of 2026-11-06 (docs/frames.md). Its frames used 100,
 * 29,891, 194,468 and 36,247 execution gas. The claim frame creates two slots (the revealed share,
 * the credit) at 97,920 state gas each, though its receipt shows one: the withdrawal empties the
 * credit, and ethrex takes that refund off the claim frame's receipt. The withdrawal creates the
 * recipient's token balance. Prefix: 2,800 for the signature + 20,000 + 60,000 <= 100,000.
 */
export const CLAIM_LIMITS = {
  verify: 20_000n, pay: 60_000n, payState: 0n,
  claim: 400_000n, claimState: 300_000n,
  withdraw: 100_000n, withdrawState: 150_000n,
};

export { profile, frame, newTx, secp256k1Placeholder, signTx, encodeTx, txHash, sigHash, toWire };

export function env() {
  const file = path.join(ROOT, ".env.frames");
  const out = {};
  for (const line of fs.readFileSync(file, "utf8").split("\n")) {
    const m = line.match(/^([A-Z0-9_]+)=(.*)$/);
    if (m) out[m[1]] = m[2];
  }
  return out;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export class RpcError extends Error {
  constructor(code, message, data) {
    super(message);
    this.code = code;
    this.data = data;
  }
}

export async function rpc(url, method, params = []) {
  for (let attempt = 0; ; attempt++) {
    let body;
    try {
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
        signal: AbortSignal.timeout(60_000),
      });
      body = await res.json();
    } catch (e) {
      if (attempt < 3) {
        await sleep(2000);
        continue;
      }
      throw new RpcError("UNREACHABLE", `${method}: ${e.message}`);
    }
    if (body.error) throw new RpcError(body.error.code, body.error.message, body.error.data);
    return body.result;
  }
}

export const artifact = (file, name) =>
  JSON.parse(fs.readFileSync(path.join(ROOT, file, `${name}.sol`, `${name}.json`), "utf8"));
export const zecswapArtifact = (name) => artifact("contracts/out", name);
export const framesArtifact = (name) => artifact("frames/contracts/out", name);

export async function waitReceipt(hash, timeoutMs = 180_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const r = await rpc(ETHREX, "eth_getTransactionReceipt", [hash]);
    if (r) return r;
    await sleep(2000);
  }
  throw new Error(`${hash} not mined in ${timeoutMs / 1000} s`);
}

/** A legacy transaction from `key`, at the pending nonce, mined and successful. */
export async function sendLegacy(key, { to = null, data = "0x", value = 0n, gas }) {
  const account = privateKeyToAccount(key);
  if (gas === undefined) {
    const estimate = await rpc(ETHREX, "eth_estimateGas", [
      { from: account.address, ...(to ? { to } : {}), data, value: `0x${value.toString(16)}` },
    ]);
    gas = (BigInt(estimate) * 11n) / 10n;
  }
  const nonce = Number(await rpc(ETHREX, "eth_getTransactionCount", [account.address, "pending"]));
  const raw = await account.signTransaction({
    chainId: CHAIN_ID, nonce, to, data, value, gas, gasPrice: LEGACY_GAS_PRICE, type: "legacy",
  });
  const hash = await rpc(ETHREX, "eth_sendRawTransaction", [raw]);
  const receipt = await waitReceipt(hash);
  if (receipt.status !== "0x1") throw new Error(`${hash} reverted (gas ${gas}, used ${BigInt(receipt.gasUsed)})`);
  return receipt;
}

export async function deploy(key, art, args = [], value = 0n) {
  const data = encodeDeployData({ abi: art.abi, bytecode: art.bytecode.object, args });
  const receipt = await sendLegacy(key, { data, value });
  return { address: receipt.contractAddress, hash: receipt.transactionHash, gasUsed: BigInt(receipt.gasUsed), block: BigInt(receipt.blockNumber) };
}

export async function call(to, abi, functionName, args = [], block = "latest") {
  const { decodeFunctionResult } = await import("viem");
  const data = encodeFunctionData({ abi, functionName, args });
  const out = await rpc(ETHREX, "eth_call", [{ to, data }, block]);
  return decodeFunctionResult({ abi, functionName, data: out });
}

export const hashTerms = (terms) =>
  keccak256(
    encodeAbiParameters(
      [{ type: "tuple", components: TERMS_COMPONENTS }],
      [terms],
    ),
  );

export const TERMS_COMPONENTS = [
  { name: "maker", type: "address" },
  { name: "token", type: "address" },
  { name: "amount", type: "uint128" },
  { name: "makerKey", type: "uint256[2]" },
  { name: "userKey", type: "uint256[2]" },
  { name: "user", type: "address" },
  { name: "t0", type: "uint64" },
  { name: "t1", type: "uint64" },
  { name: "payoutNote", type: "bytes32" },
];

/**
 * The sponsored claim (docs/frames.md): the swap's own key verifies execution, the sponsor pays,
 * then the claim and the withdrawal run as two separate SENDER frames, never one atomic batch.
 * `extra` frames go between the validation prefix and the claim.
 */
export function claimTx({ user, sponsor, swaps, sponsorData, claimData, withdrawData, limits, extra = [], maxFeePerGas = FRAME_MAX_FEE, nonce = 0n }) {
  return newTx({
    chainId: CHAIN_ID,
    sender: user,
    nonceSeq: nonce,
    fees: { maxFeePerGas },
    signatures: [secp256k1Placeholder(profile)],
    frames: [
      frame(profile, "VERIFY", { scope: "EXECUTION", execution: limits.verify }),
      frame(profile, "VERIFY", { scope: "PAYMENT", target: sponsor, execution: limits.pay, state: limits.payState ?? 0n, data: sponsorData }),
      ...extra,
      frame(profile, "SENDER", { target: swaps, execution: limits.claim, state: limits.claimState, data: claimData }),
      frame(profile, "SENDER", { target: swaps, execution: limits.withdraw, state: limits.withdrawState, data: withdrawData }),
    ],
  });
}
