// Proves private Railgun sends and withdrawals the way a wallet that uses a relayer as its
// broadcaster does, with Railgun's own wallet SDK, for the live suite (crates/zecswap-e2e):
//   node send.cjs wallet               a new wallet: { mnemonic, seed }
//   node send.cjs prove < request      each send's transaction: { chainId, to, data, value }
//   node send.cjs balances < request   each wallet's balance of a token
// Requests come on stdin as JSON, since they carry mnemonics and seeds; nothing secret is
// printed but the new wallet. On an anvil fork of Ethereum Sepolia, screening (POI) is off and
// Railgun's history is taken up to the fork block, after which the fork's own blocks are read.
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const memdown = require('memdown');
const snarkjs = require('snarkjs');
const W = require('@railgun-community/wallet');
const {
  NetworkName,
  NETWORK_CONFIG,
  TXIDVersion,
  getEVMGasTypeForTransaction,
} = require('@railgun-community/shared-models');
const { Mnemonic } = require('@railgun-community/engine');
const { wallet, shareableViewingKey } = require('./engine.cjs');

const NETWORK = NetworkName.EthereumSepolia;
const POI_NODE = 'https://ppoi.fdi.network/';
const V2 = TXIDVersion.V2_PoseidonMerkle;
const { chain } = NETWORK_CONFIG[NETWORK];
const started = Date.now();
/** Progress on stderr, which the live suite shows when a step fails. */
const log = (message) => console.error(`${((Date.now() - started) / 1000).toFixed(0)}s ${message}`);

async function rpc(url, method, params = []) {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  return (await response.json()).result;
}

/** Proving keys, downloaded once into `dir`. */
function artifactStore(dir) {
  const at = (file) => path.join(dir, file);
  return new W.ArtifactStore(
    (file) => fs.promises.readFile(at(file)),
    async (folder, file, item) => {
      await fs.promises.mkdir(at(folder), { recursive: true });
      await fs.promises.writeFile(at(file), item);
    },
    async (file) => fs.existsSync(at(file)),
  );
}

/**
 * Starts the engine on `url`. A fork's notes never reach the screening nodes, and the quick-sync
 * history runs past the fork into blocks the fork never had: it is cut at the fork block, and
 * the engine scans on from there.
 */
async function start(url, artifacts) {
  const fork = (await rpc(url, 'anvil_metadata').catch(() => undefined))?.forkedNetwork?.forkBlockNumber;
  if (fork !== undefined) {
    delete NETWORK_CONFIG[NETWORK].poi;
  }
  await W.startRailgunEngine('zecswap e2e', memdown(), false, artifactStore(artifacts), false, false, [POI_NODE]);
  W.getProver().setSnarkJSGroth16(snarkjs.groth16);
  if (fork !== undefined) {
    const engine = W.getEngine();
    const quickSync = engine.quickSyncEvents;
    engine.quickSyncEvents = async (txidVersion, syncChain, startingBlock) => {
      const events = await quickSync(txidVersion, syncChain, startingBlock);
      await engine.setLastSyncedBlock(txidVersion, syncChain, fork);
      const past = (event) => Number(event.blockNumber) <= fork;
      return {
        ...events,
        commitmentEvents: events.commitmentEvents.filter(past),
        unshieldEvents: events.unshieldEvents.filter(past),
        nullifierEvents: events.nullifierEvents.filter(past),
      };
    };
  }
  let scanned = -1;
  W.setOnUTXOMerkletreeScanCallback(({ scanStatus, progress }) => {
    const percent = Math.floor(progress * 4) * 25;
    if (percent !== scanned || scanStatus !== 'Updated') log(`note tree ${scanStatus} ${percent}%`);
    scanned = percent;
  });
  const provider = { provider: url, priority: 1, weight: 2, maxLogsPerBatch: 10, stallTimeout: 2500 };
  await W.loadProvider({ chainId: chain.id, providers: [provider] }, NETWORK, 10_000);
  log(fork === undefined ? 'started' : `started on a fork at block ${fork}`);
  return { fork: fork !== undefined };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Each wallet's latest balances by bucket, and how often it reported them. */
const buckets = new Map();
const reports = new Map();
W.setOnBalanceUpdateCallback(({ railgunWalletID, balanceBucket, erc20Amounts }) => {
  if (!buckets.has(railgunWalletID)) buckets.set(railgunWalletID, new Map());
  buckets.get(railgunWalletID).set(balanceBucket, erc20Amounts);
  reports.set(railgunWalletID, (reports.get(railgunWalletID) ?? 0) + 1);
});

/**
 * Each wallet's unspent balance of `token`, in all and spendable, after a scan. A wallet
 * reports its balances once it has decrypted the scan, after the scan itself returns.
 */
async function balances(ids, token) {
  const before = ids.map((id) => reports.get(id) ?? 0);
  await W.refreshBalances(chain, ids);
  for (let waited = 0; ids.some((id, i) => (reports.get(id) ?? 0) === before[i]); waited += 1) {
    if (waited === 120) throw new Error('the SDK reported no balances after its scan');
    await sleep(1000);
  }
  await sleep(1000);
  return ids.map((id) => {
    const balance = { total: 0n, spendable: 0n };
    for (const [bucket, amounts] of buckets.get(id) ?? []) {
      const amount = amounts.find((a) => a.tokenAddress.toLowerCase() === token.toLowerCase())?.amount ?? 0n;
      if (bucket !== 'Spent') balance.total += amount;
      if (bucket === 'Spendable') balance.spendable += amount;
    }
    return balance;
  });
}

/**
 * Waits until the sender can spend `needed`: on a live chain, once screening clears its notes.
 * The wallet submits the screening proofs of its own sends, as the SDK does after each scan.
 */
async function spendable(id, token, needed, fork, waitSeconds) {
  const deadline = Date.now() + waitSeconds * 1000;
  for (;;) {
    const [balance] = await balances([id], token);
    const available = fork ? balance.total : balance.spendable;
    const held = [...(buckets.get(id) ?? [])]
      .map(([bucket, amounts]) => [bucket, amounts.find((a) => a.tokenAddress.toLowerCase() === token.toLowerCase())?.amount ?? 0n])
      .filter(([, amount]) => amount > 0n)
      .map(([bucket, amount]) => `${amount} ${bucket}`);
    log(`${available} spendable of ${held.join(', ') || 'nothing'}; ${needed} needed`);
    if (available >= needed) return available;
    if (Date.now() > deadline) throw new Error(`only ${available} of ${needed} became spendable`);
    if (!fork) {
      await W.generatePOIsForWallet(NETWORK, id).catch((e) => log(`screening proofs: ${e.message}`));
      await W.refreshReceivePOIsForWallet(V2, NETWORK, id).catch((e) => log(`screening status: ${e.message}`));
    }
    await sleep(10_000);
  }
}

async function prove(request) {
  const { fork } = await start(request.rpc, request.artifacts);
  const encryptionKey = crypto.randomBytes(32).toString('hex');
  const sender = await W.createRailgunWallet(encryptionKey, request.mnemonic, {
    [NETWORK]: request.creationBlock,
  });
  // Every send is proved from the same notes, so the largest must fit.
  const needed = request.sends
    .map((send) => BigInt(send.amount) + BigInt(send.fee ?? request.broadcaster.fee))
    .reduce((most, need) => (need > most ? need : most), 0n);
  const available = await spendable(sender.id, request.token, needed, fork, request.waitSeconds ?? 600);

  const minGasPrice = BigInt(request.minGasPrice);
  const gasDetails = {
    evmGasType: getEVMGasTypeForTransaction(NETWORK, false),
    gasEstimate: 1_000_000n,
    gasPrice: minGasPrice,
  };
  const transactions = [];
  for (const send of request.sends) {
    const fee = {
      tokenAddress: request.broadcaster.token,
      amount: BigInt(send.fee ?? request.broadcaster.fee),
      recipientAddress: request.broadcaster.railgunAddress,
    };
    const recipients = [{ tokenAddress: request.token, amount: BigInt(send.amount), recipientAddress: send.to }];
    let populated;
    log(`proving ${send.to.startsWith('0zk') ? 'a private send' : 'a withdrawal'}`);
    if (send.to.startsWith('0zk')) {
      await W.generateTransferProof(V2, NETWORK, sender.id, encryptionKey, false, undefined, recipients, [], fee, false, minGasPrice, () => {});
      populated = await W.populateProvedTransfer(V2, NETWORK, sender.id, false, undefined, recipients, [], fee, false, minGasPrice, gasDetails);
    } else {
      await W.generateUnshieldProof(V2, NETWORK, sender.id, encryptionKey, recipients, [], fee, false, minGasPrice, () => {});
      populated = await W.populateProvedUnshield(V2, NETWORK, sender.id, recipients, [], fee, false, minGasPrice, gasDetails);
    }
    const { to, data, value } = populated.transaction;
    transactions.push({ chainId: chain.id, to, data, value: (value ?? 0n).toString() });
  }
  return { address: sender.railgunAddress, available: available.toString(), transactions };
}

async function balancesOf(request) {
  await start(request.rpc, request.artifacts);
  const ids = [];
  for (const seed of request.seeds) {
    const keys = await wallet(seed, 0);
    const info = await W.createViewOnlyRailgunWallet(
      crypto.randomBytes(32).toString('hex'),
      shareableViewingKey(keys),
      { [NETWORK]: request.creationBlock },
    );
    ids.push(info.id);
  }
  const found = await balances(ids, request.token);
  return { balances: found.map(({ total, spendable }) => ({ total: total.toString(), spendable: spendable.toString() })) };
}

async function main() {
  const command = process.argv[2];
  if (command === 'wallet') {
    const mnemonic = Mnemonic.generate();
    return { mnemonic, seed: `0x${Mnemonic.toSeed(mnemonic, '')}` };
  }
  const request = JSON.parse(fs.readFileSync(0, 'utf8'));
  if (command === 'prove') return prove(request);
  if (command === 'balances') return balancesOf(request);
  throw new Error('usage: node send.cjs wallet | prove | balances');
}

main().then(
  async (result) => {
    process.stdout.write(`${JSON.stringify(result)}\n`);
    await W.stopRailgunEngine().catch(() => {});
    process.exit(0);
  },
  (error) => {
    console.error(error.message);
    process.exit(1);
  },
);
