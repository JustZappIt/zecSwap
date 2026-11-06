// The go/no-go probe on the EF frames devnet: deploys tUSD, ZecSwap, a TXPARAM probe and a
// ClaimSponsor, opens and readies a swap whose user key is a test vector's (its share is public),
// and sends the sponsored claim to the canonical mempool, the alt mempool unstaked, and the alt
// mempool staked. With --deployment it claims such a swap on deployments/frames-devnet.json's
// contracts instead, at the claim's fixed limits. Resumable: it keeps what it did in runs/.
// Writes a JSON report there.
//   node scripts/probe.mjs [--fresh] [--deployment]
import fs from "node:fs";
import path from "node:path";
import { encodeFunctionData, decodeEventLog, parseEther, maxUint256 } from "viem";
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts";
import {
  ROOT, CHAIN_ID, ETHREX, MEMPOOL, explorerLink, REGISTRY, REGISTRY_ABI, MANIFEST, CLAIM_LIMITS, FRAME_MAX_FEE, env, rpc, RpcError, waitReceipt,
  sendLegacy, deploy, call, zecswapArtifact, framesArtifact, hashTerms, claimTx, profile, frame, newTx,
  secp256k1Placeholder, signTx, encodeTx, txHash, toWire,
} from "./devnet.mjs";

const fresh = process.argv.includes("--fresh");
const deployed = process.argv.includes("--deployment") ? JSON.parse(fs.readFileSync(MANIFEST, "utf8")) : null;
const STATE = path.join(ROOT, `frames/runs/${deployed ? "smoke" : "probe"}.json`);
const keys = env();
const deployer = keys.FRAMES_DEPLOYER_KEY;
const maker = keys.MAKER_PRIVATE_KEY;
const makerAddress = privateKeyToAccount(maker).address;

const log = (...a) => console.error(...a);
fs.mkdirSync(path.dirname(STATE), { recursive: true });
let state = !fresh && fs.existsSync(STATE) ? JSON.parse(fs.readFileSync(STATE, "utf8")) : {};
const save = (patch) => {
  state = { ...state, ...patch };
  fs.writeFileSync(STATE, JSON.stringify(state, (_, v) => (typeof v === "bigint" ? v.toString() : v), 2));
};

const tokenArt = zecswapArtifact("TestToken");
const swapsArt = zecswapArtifact("ZecSwap");
const sponsorArt = framesArtifact("ClaimSponsor");
const probeArt = framesArtifact("Probe");

// --- the chain, and a reset since the last run
if (Number(await rpc(ETHREX, "eth_chainId")) !== CHAIN_ID) throw new Error("not chain 1337");
const block1 = (await rpc(ETHREX, "eth_getBlockByNumber", ["0x1", false])).hash;
if (state.block1 && state.block1 !== block1) {
  log("the devnet was reset since the last run: starting over");
  state = {};
}
save({ block1 });

// --- contracts
async function ensure(name, art, args, value = 0n) {
  if (state[name]) return state[name];
  log(`deploying ${name}…`);
  const d = await deploy(deployer, art, args, value);
  log(`  ${name} ${d.address}, ${d.gasUsed} gas, block ${d.block}`);
  save({ [name]: d.address, [`${name}Deploy`]: { hash: d.hash, gasUsed: d.gasUsed, block: d.block } });
  return d.address;
}
const token = deployed?.token ?? (await ensure("token", tokenArt, []));
const swaps = deployed?.contract ?? (await ensure("swaps", swapsArt, [1800n, 3600n, "0x0000000000000000000000000000000000000000"]));
const probe = deployed ? null : await ensure("probe", probeArt, []);

// --- (a) TXPARAM in a self-paid transaction from the deployer, whose nonce is not zero
async function decodeProbe(receipt) {
  const out = { caller: null, params: {} };
  for (const l of receipt.logs ?? []) {
    if (l.address.toLowerCase() !== probe.toLowerCase()) continue;
    const ev = decodeEventLog({ abi: probeArt.abi, data: l.data, topics: l.topics });
    if (ev.eventName === "Caller") out.caller = ev.args.caller;
    else out.params[`0x${ev.args.id.toString(16).padStart(2, "0")}`] = ev.args.ok ? `0x${ev.args.value.toString(16)}` : "failed";
  }
  return out;
}
if (!deployed && !state.selfPaid) {
  const account = privateKeyToAccount(deployer);
  const nonce = BigInt(await rpc(ETHREX, "eth_getTransactionCount", [account.address, "latest"]));
  const tx = await signTx(profile, newTx({
    chainId: CHAIN_ID, sender: account.address, nonceSeq: nonce, fees: { maxFeePerGas: FRAME_MAX_FEE },
    signatures: [secp256k1Placeholder(profile)],
    frames: [
      frame(profile, "VERIFY", { scope: "EXECUTION_AND_PAYMENT", execution: 30_000n }),
      frame(profile, "DEFAULT", { target: probe, execution: 1_500_000n }),
    ],
  }), deployer);
  log(`TXPARAM probe, self-paid, sender nonce ${nonce}…`);
  const hash = await rpc(MEMPOOL, "eth_sendFrameTransaction", [toWire(tx)]);
  const receipt = await waitReceipt(hash);
  save({ selfPaid: { hash, nonce, sender: account.address, maxFeePerGas: FRAME_MAX_FEE, status: receipt.status, frames: receipt.frameReceipts, gasUsed: receipt.gasUsed, ...(await decodeProbe(receipt)) } });
}

// --- the sponsor, unstaked for now
const MAX_COST = parseEther("0.01");
const sponsor = deployed?.sponsor.address ?? (await ensure("sponsor", sponsorArt, [swaps, makerAddress, REGISTRY, MAX_COST], parseEther("0.05")));

// --- a swap of the maker's whose user key is test vector 260 (k public), maker key vector 261
const vectors = JSON.parse(fs.readFileSync(path.join(ROOT, "contracts/test/vectors/spend_auth_g.json"), "utf8")).vectors;
const z = vectors[260], e = vectors[261];
if (BigInt(z.x) === BigInt(e.x)) throw new Error("vectors share an x");
const AMOUNT = 5_000_000n;
if (!state.swap) {
  const userKey = generatePrivateKey();
  const user = privateKeyToAccount(userKey).address;
  const swapsAbi = swapsArt.abi;
  log(`maker ${makerAddress}: mint, approve, deposit…`);
  await sendLegacy(maker, { to: token, data: encodeFunctionData({ abi: tokenArt.abi, functionName: "mint", args: [makerAddress, 100n * AMOUNT] }) });
  await sendLegacy(maker, { to: token, data: encodeFunctionData({ abi: tokenArt.abi, functionName: "approve", args: [swaps, maxUint256] }) });
  await sendLegacy(maker, { to: swaps, data: encodeFunctionData({ abi: swapsAbi, functionName: "deposit", args: [token, 100n * AMOUNT] }) });
  const now = BigInt((await rpc(ETHREX, "eth_getBlockByNumber", ["latest", false])).timestamp);
  const terms = {
    maker: makerAddress, token, amount: AMOUNT, makerKey: [BigInt(e.x), BigInt(e.y)], userKey: [BigInt(z.x), BigInt(z.y)],
    user, t0: now + 2100n, t1: now + 2100n + 1860n, payoutNote: `0x${"00".repeat(32)}`,
  };
  log(`opening for user ${user}…`);
  const opened = await sendLegacy(maker, { to: swaps, data: encodeFunctionData({ abi: swapsAbi, functionName: "open", args: [token, AMOUNT, terms.makerKey, terms.userKey, user, terms.t0, terms.t1, terms.payoutNote] }) });
  const id = await call(swaps, swapsAbi, "swapId", [makerAddress, terms.userKey]);
  const readied = await sendLegacy(maker, { to: swaps, data: encodeFunctionData({ abi: swapsAbi, functionName: "ready", args: [id, terms] }) });
  save({ swap: { id, terms, userKey, opened: opened.transactionHash, openGas: BigInt(opened.gasUsed), ready: readied.transactionHash } });
}
const { id, userKey } = state.swap;
const terms = { ...state.swap.terms, amount: BigInt(state.swap.terms.amount), t0: BigInt(state.swap.terms.t0), t1: BigInt(state.swap.terms.t1), makerKey: state.swap.terms.makerKey.map(BigInt), userKey: state.swap.terms.userKey.map(BigInt) };
const user = terms.user;
if (hashTerms(terms) !== (await call(swaps, swapsArt.abi, "getSwap", [id])).termsHash) throw new Error("terms don't hash to the swap's");

// --- the claim, with a DEFAULT frame reporting TXPARAM as the sponsor sees it, and limits wide
// enough to measure what each frame uses
const sponsorData = encodeFunctionData({ abi: sponsorArt.abi, functionName: "sponsor", args: [id, terms] });
const claimData = encodeFunctionData({ abi: swapsArt.abi, functionName: "claim", args: [id, terms, BigInt(z.k)] });
const withdrawData = encodeFunctionData({ abi: swapsArt.abi, functionName: "withdraw", args: [token, AMOUNT, user] });
const limits = deployed ? CLAIM_LIMITS : { verify: 20_000n, pay: 60_000n, payState: 0n, claim: 400_000n, claimState: 600_000n, withdraw: 100_000n, withdrawState: 300_000n };
const build = () => signTx(profile, claimTx({
  user, sponsor, swaps, sponsorData, claimData, withdrawData, limits,
  extra: deployed ? [] : [frame(profile, "DEFAULT", { target: probe, execution: 1_500_000n })],
}), userKey);
const tx = await build();
const raw = encodeTx(profile, tx);
const hash = txHash(profile, tx);
save({ claimHash: hash, claimLimits: limits });

const report = { ...state };
const attempt = async (f) => {
  try {
    return { accepted: true, result: await f() };
  } catch (e) {
    if (!(e instanceof RpcError)) throw e;
    return { accepted: false, code: e.code, message: e.message, data: e.data };
  }
};

if (!state.claimed) {
  log("simulating (test vector's share)…");
  report.simulation = await attempt(() => rpc(ETHREX, "ethrex_simulateFrameTransaction", [raw, "latest", { trace: true }]));
  log("canonical mempool (ethrex eth_sendRawTransaction)…");
  if (!deployed) report.canonical = await attempt(() => rpc(ETHREX, "eth_sendRawTransaction", [raw]));
  const stakeNow = (await call(REGISTRY, REGISTRY_ABI, "getDepositInfo", [sponsor]))[0];
  if (stakeNow === 0n) {
    log("alt mempool, sponsor unstaked…");
    report.altUnstaked = await attempt(() => rpc(MEMPOOL, "eth_sendFrameTransaction", [toWire(tx)]));
    report.altUnstakedAdmission = await attempt(() => rpc(MEMPOOL, "skandha_explorer_getTransaction", [hash]));
    log("staking the sponsor…");
    const staked = await sendLegacy(deployer, { to: sponsor, value: parseEther("0.012"), data: encodeFunctionData({ abi: sponsorArt.abi, functionName: "stake", args: [86400] }) });
    save({ stake: { hash: staked.transactionHash } });
    report.stake = state.stake;
  }
  report.stakeInfo = (await call(REGISTRY, REGISTRY_ABI, "getDepositInfo", [sponsor])).map(String);
  const before = BigInt(await rpc(ETHREX, "eth_getBalance", [sponsor, "latest"]));
  log("alt mempool, sponsor staked…");
  report.altStaked = await attempt(() => rpc(MEMPOOL, "eth_sendFrameTransaction", [toWire(tx)]));
  if (report.altStaked.accepted) {
    const receipt = await waitReceipt(hash);
    const after = BigInt(await rpc(ETHREX, "eth_getBalance", [sponsor, "latest"]));
    save({ claimed: { hash, block: BigInt(receipt.blockNumber), status: receipt.status, gasUsed: receipt.gasUsed, frames: receipt.frameReceipts, sponsorPaid: before - after, ...(deployed ? {} : { probe: await decodeProbe(receipt) }) } });
  }
}
report.claimed = state.claimed;
report.status = await attempt(() => rpc(MEMPOOL, "skandha_frameTransactionStatus", [hash]));
report.frameReceipt = await attempt(() => rpc(MEMPOOL, "eth_getFrameTransactionReceipt", [hash]));
report.admission = await attempt(() => rpc(MEMPOOL, "skandha_explorer_getTransaction", [hash]));
report.after = {
  swap: await call(swaps, swapsArt.abi, "getSwap", [id]),
  credit: await call(swaps, swapsArt.abi, "balanceOf", [user, token]),
  userTokens: await call(token, tokenArt.abi, "balanceOf", [user]),
  userEth: BigInt(await rpc(ETHREX, "eth_getBalance", [user, "latest"])),
};
report.explorer = explorerLink(hash);
const out = path.join(ROOT, `frames/runs/${deployed ? "smoke" : "probe"}-report-${Date.now()}.json`);
fs.writeFileSync(out, JSON.stringify(report, (_, v) => (typeof v === "bigint" ? v.toString() : v), 2));
log(`report: ${out}`);
