// Known answers from Railgun's engine for zecswap-railgun's tests:
//   node vectors.cjs > ../tests/engine-vectors.json
const {
  wallet,
  deriveNodes,
  Mnemonic,
  decryptRandom,
  getSharedSymmetricKey,
  getPublicViewingKey,
  ShieldNote,
  ShieldNoteERC20,
  hex,
  bytes,
  word,
} = require('./engine.cjs');

// The engine's own key-derivation test mnemonics.
const MNEMONICS = [
  'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about',
  'culture flower sunny seat maximum begin design magnet side permit coin dial alter insect whisper series desk power cream afford regular strike poem ostrich',
];
const USDC_SEPOLIA = '0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238';

async function main() {
  const wallets = [];
  for (const mnemonic of MNEMONICS) {
    for (const index of [0, 1]) {
      const keys = await wallet(`0x${Mnemonic.toSeed(mnemonic, '')}`, index);
      const nodes = deriveNodes(mnemonic, index);
      if (hex(nodes.spending.getSpendingKeyPair().privateKey) !== keys.spendingPrivateKey) {
        throw new Error('the seed and mnemonic routes disagree');
      }
      wallets.push(keys);
    }
  }

  // Shields built by the engine, each with fresh random IVs, to the first two wallets.
  const notes = [];
  for (const [n, receiver] of wallets.slice(0, 2).entries()) {
    const random = hex(Buffer.alloc(16, 0x11 * (n + 1)));
    const shieldPrivateKey = hex(Buffer.alloc(32, 0x07 + n));
    const note = new ShieldNoteERC20(BigInt(receiver.masterPublicKey), random, 1000000n, USDC_SEPOLIA);
    const request = await note.serialize(bytes(shieldPrivateKey), bytes(receiver.viewingPublicKey));
    const { encryptedBundle, shieldKey } = request.ciphertext;
    const sharedKey = await getSharedSymmetricKey(bytes(shieldPrivateKey), bytes(receiver.viewingPublicKey));
    const decrypted = await decryptRandom(receiver.viewingPrivateKey, encryptedBundle, shieldKey);
    if (decrypted !== random.slice(2)) throw new Error('the engine cannot decrypt its own note');
    notes.push({
      receiver: n,
      random,
      shieldPrivateKey,
      shieldKey: hex(await getPublicViewingKey(bytes(shieldPrivateKey))),
      sharedKey: hex(sharedKey),
      npk: word(ShieldNote.getNotePublicKey(BigInt(receiver.masterPublicKey), random)),
      encryptedBundle,
    });
  }

  process.stdout.write(`${JSON.stringify({ engine: '9.8.0', wallets, notes }, null, 2)}\n`);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
