// Syncs the Railgun wallet a seed opens on Ethereum Sepolia, with Railgun's own wallet SDK, and
// prints its balances by screening status: payouts land where Railgun's wallets find them, and
// clear screening. The seed is read from a file and never printed.
//   ETH_SEPOLIA_RPC_URL=… node balance.cjs <seed-file> [scan-from-block]
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');
const memdown = require('memdown');
const msgpack = require('msgpack-lite');
const W = require('@railgun-community/wallet');
const { NetworkName, NETWORK_CONFIG, TXIDVersion } = require('@railgun-community/shared-models');
const { wallet, Babyjubjub } = require('./engine.cjs');

const NETWORK = NetworkName.EthereumSepolia;
const POI_NODE = 'https://ppoi.fdi.network/';

/** What Railgun's view-only wallets are made from: the viewing key and packed spending key. */
function shareableViewingKey(keys) {
  const spub = Babyjubjub.packPoint(keys.spendingPublicKey.map(BigInt)).toString('hex');
  const vpriv = keys.viewingPrivateKey.replace(/^0x/, '');
  return msgpack.encode({ vpriv, spub }).toString('hex');
}

/** Runs `refresh` and returns the balances by bucket it reports, all in one pass. */
function balancesAfter(refresh) {
  return new Promise((resolve, reject) => {
    const balances = new Map();
    W.setOnBalanceUpdateCallback(({ balanceBucket, erc20Amounts }) => {
      balances.set(balanceBucket, erc20Amounts);
      setTimeout(() => resolve(balances), 1000);
    });
    refresh().catch(reject);
  });
}

/** Artifacts are for proving, which a balance never needs; they go to a scratch directory. */
function scratchArtifacts() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'railgun-artifacts-'));
  const at = (file) => path.join(root, file);
  return new W.ArtifactStore(
    (file) => fs.promises.readFile(at(file)),
    async (dir, file, item) => {
      await fs.promises.mkdir(at(dir), { recursive: true });
      await fs.promises.writeFile(at(file), item);
    },
    async (file) => fs.existsSync(at(file)),
  );
}

async function main() {
  const [seedFile, fromBlock] = process.argv.slice(2);
  const rpc = process.env.ETH_SEPOLIA_RPC_URL;
  if (!seedFile || !rpc) {
    throw new Error('usage: ETH_SEPOLIA_RPC_URL=… node balance.cjs <seed-file> [scan-from-block]');
  }
  const seed = `0x${fs.readFileSync(seedFile, 'utf8').trim().replace(/^0x/, '')}`;
  const keys = await wallet(seed, 0);

  await W.startRailgunEngine('zecswapcheck', memdown(), false, scratchArtifacts(), false, false, [POI_NODE]);
  let lastProgress = -1;
  let scan = 'not started';
  W.setOnUTXOMerkletreeScanCallback(({ scanStatus, progress }) => {
    const percent = Math.floor(progress * 10) * 10;
    if (percent !== lastProgress) console.error(`note tree ${scanStatus} ${percent}%`);
    lastProgress = percent;
    scan = scanStatus;
  });
  const { chain } = NETWORK_CONFIG[NETWORK];
  const provider = { provider: rpc, priority: 1, weight: 2, maxLogsPerBatch: 10, stallTimeout: 2500 };
  await W.loadProvider({ chainId: chain.id, providers: [provider] }, NETWORK, 10_000);

  const creation = fromBlock ? { [NETWORK]: Number(fromBlock) } : undefined;
  const encryptionKey = crypto.randomBytes(32).toString('hex');
  const info = await W.createViewOnlyRailgunWallet(encryptionKey, shareableViewingKey(keys), creation);
  if (info.railgunAddress !== keys.address) {
    throw new Error(`the SDK opened ${info.railgunAddress}, not ${keys.address}`);
  }
  await balancesAfter(() => W.refreshBalances(chain, [info.id]));
  if (scan !== 'Complete') {
    throw new Error(`the note tree scan ended ${scan}: try an RPC that serves wider log ranges`);
  }
  // A new wallet finds its notes before it asks the screening node what became of them.
  await W.refreshReceivePOIsForWallet(TXIDVersion.V2_PoseidonMerkle, NETWORK, info.id);
  const balances = await balancesAfter(() => W.refreshBalances(chain, [info.id]));

  console.log(`wallet ${info.railgunAddress}`);
  const lines = [...balances].flatMap(([bucket, amounts]) =>
    amounts.map(({ tokenAddress, amount }) => `${bucket.padEnd(20)} ${amount} of ${tokenAddress}`),
  );
  console.log(lines.length > 0 ? lines.join('\n') : 'no balances');
  await W.stopRailgunEngine();
}

main().then(
  () => process.exit(0),
  (error) => {
    console.error(error);
    process.exit(1);
  },
);
