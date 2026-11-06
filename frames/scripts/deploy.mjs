// Deploys the frames devnet's contracts in one command, from the keys in .env.frames: tUSD,
// ZecSwap with 30-minute and 1-hour windows paying accounts (no Railgun), and a ClaimSponsor for
// the maker, funded through its creation and staked in the registry. Reads them back and writes
// deployments/frames-devnet.json with block 1's hash, as the devnet is reset now and then: a
// manifest whose hash no longer matches is deployed over.
//   node scripts/deploy.mjs [--fresh]
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { encodeFunctionData, parseEther, zeroAddress } from "viem";
import {
  ROOT, CHAIN_ID, ETHREX, MEMPOOL, REGISTRY, REGISTRY_ABI, MANIFEST, env, rpc, sendLegacy, deploy, call,
  zecswapArtifact, framesArtifact,
} from "./devnet.mjs";

const MIN_WINDOW = 1800n;
const LATE_WINDOW = 3600n;
/** About 2.5 times what a claim at the fixed limits may cost at 2 gwei. */
const MAX_COST = parseEther("0.005");
const FUNDING = parseEther("0.2");
const STAKE = parseEther("0.012");
const UNSTAKE_DELAY = 86400;

const keys = env();
if (Number(await rpc(ETHREX, "eth_chainId")) !== CHAIN_ID) throw new Error("the RPC is not the frames devnet");
const block1Hash = (await rpc(ETHREX, "eth_getBlockByNumber", ["0x1", false])).hash;
if (fs.existsSync(MANIFEST) && !process.argv.includes("--fresh")) {
  const live = JSON.parse(fs.readFileSync(MANIFEST, "utf8"));
  if (live.block1Hash === block1Hash && (await rpc(ETHREX, "eth_getCode", [live.contract, "latest"])) !== "0x") {
    throw new Error(`${path.relative(ROOT, MANIFEST)} is live on this devnet; --fresh deploys again`);
  }
}
const git = (...args) => execFileSync("git", args, { cwd: ROOT, encoding: "utf8" }).trim();
if (git("status", "--porcelain", "--", "contracts/src", "contracts/test/utils/TestToken.sol", "frames/contracts/src")) {
  throw new Error("the contracts have uncommitted changes: deploy committed code");
}
for (const dir of ["contracts", "frames/contracts"]) {
  execFileSync("forge", ["build", "--quiet"], { cwd: path.join(ROOT, dir), stdio: "inherit", env: { ...process.env, FOUNDRY_DISABLE_NIGHTLY_WARNING: "1" } });
}

const key = keys.FRAMES_DEPLOYER_KEY;
const sponsorArt = framesArtifact("ClaimSponsor");
const swapsArt = zecswapArtifact("ZecSwap");
console.error("deploying tUSD, ZecSwap and the sponsor…");
const token = await deploy(key, zecswapArtifact("TestToken"));
const swaps = await deploy(key, swapsArt, [MIN_WINDOW, LATE_WINDOW, zeroAddress]);
const sponsor = await deploy(key, sponsorArt, [swaps.address, keys.FRAMES_MAKER, REGISTRY, MAX_COST], FUNDING);
console.error("staking the sponsor…");
const staked = await sendLegacy(key, {
  to: sponsor.address,
  value: STAKE,
  data: encodeFunctionData({ abi: sponsorArt.abi, functionName: "stake", args: [UNSTAKE_DELAY] }),
});

const read = (to, abi, fn, args = []) => call(to, abi, fn, args);
const expect = (what, got, want) => {
  if (String(got).toLowerCase() !== String(want).toLowerCase()) throw new Error(`${what} reads back ${got}, not ${want}`);
};
expect("MIN_WINDOW", await read(swaps.address, swapsArt.abi, "MIN_WINDOW"), MIN_WINDOW);
expect("LATE_WINDOW", await read(swaps.address, swapsArt.abi, "LATE_WINDOW"), LATE_WINDOW);
expect("RAILGUN", await read(swaps.address, swapsArt.abi, "RAILGUN"), zeroAddress);
expect("SWAPS", await read(sponsor.address, sponsorArt.abi, "SWAPS"), swaps.address);
expect("MAKER", await read(sponsor.address, sponsorArt.abi, "MAKER"), keys.FRAMES_MAKER);
expect("MAX_COST", await read(sponsor.address, sponsorArt.abi, "MAX_COST"), MAX_COST);
const [stake, delay, withdrawTime] = await read(REGISTRY, REGISTRY_ABI, "getDepositInfo", [sponsor.address]);
expect("the sponsor's stake", `${stake}/${delay}/${withdrawTime}`, `${STAKE}/${UNSTAKE_DELAY}/0`);

const manifest = {
  chainId: CHAIN_ID,
  chain: "the EF's EIP-8141 alt-mempool devnet: frame transactions exist nowhere else (docs/frames.md)",
  contract: swaps.address,
  token: token.address,
  railgun: zeroAddress,
  minWindowSeconds: Number(MIN_WINDOW),
  lateWindowSeconds: Number(LATE_WINDOW),
  deploymentTransaction: swaps.hash,
  deploymentBlock: Number(swaps.block),
  deploymentGasUsed: Number(swaps.gasUsed),
  tokenTransaction: token.hash,
  zcashNetwork: "testnet",
  maker: keys.FRAMES_MAKER,
  sponsor: {
    address: sponsor.address,
    transaction: sponsor.hash,
    maxCostWei: MAX_COST.toString(),
    fundingWei: FUNDING.toString(),
    stake: { registry: REGISTRY, wei: STAKE.toString(), unstakeDelaySeconds: UNSTAKE_DELAY, transaction: staked.transactionHash },
  },
  rpc: ETHREX,
  mempool: MEMPOOL,
  explorer: "https://explorer.altmempool.wtf/",
  block1Hash,
  deployer: keys.FRAMES_DEPLOYER,
  sourceCommit: git("rev-parse", "HEAD"),
  deployedAt: new Date().toISOString().slice(0, 10),
  status: "active",
};
fs.writeFileSync(MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(JSON.stringify(manifest, null, 2));
