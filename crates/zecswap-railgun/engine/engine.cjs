// Railgun's own key, address and shield-note code, loaded from its engine package. Its "exports"
// map hides these modules, so they are required by path.
const path = require('path');

const dist = path.dirname(require.resolve('@railgun-community/engine'));
const load = (module) => require(path.join(dist, module));

const { deriveNodes, WalletNode } = load('key-derivation/wallet-node');
const { getMasterKeyFromSeed } = load('key-derivation/bip32');
const { encodeAddress } = load('key-derivation/bech32');
const { Babyjubjub } = load('key-derivation/babyjubjub');
const { getSharedSymmetricKey, getPublicViewingKey } = load('utils/keys-utils');
const { initPoseidonPromise } = load('utils/poseidon');
const { initCurve25519Promise } = load('utils/scalar-multiply');
const { ShieldNote, ShieldNoteERC20, Mnemonic } = require('@railgun-community/engine');

const hex = (bytes) => `0x${Buffer.from(bytes).toString('hex')}`;
const bytes = (value) => Buffer.from(value.replace(/^0x/, ''), 'hex');
const word = (n) => `0x${n.toString(16).padStart(64, '0')}`;

/**
 * A wallet's keys as Railgun derives them from a BIP-39 seed. Its wallets start from the
 * mnemonic (`deriveNodes`); `vectors.cjs` checks that both routes agree.
 */
async function wallet(seed, index) {
  await Promise.all([initPoseidonPromise, initCurve25519Promise]);
  const master = new WalletNode(getMasterKeyFromSeed(seed.replace(/^0x/, '')));
  const nodes = {
    spending: master.derive(`m/44'/1984'/0'/0'/${index}'`),
    viewing: master.derive(`m/420'/1984'/0'/0'/${index}'`),
  };
  const spending = nodes.spending.getSpendingKeyPair();
  const viewing = await nodes.viewing.getViewingKeyPair();
  const nullifyingKey = await nodes.viewing.getNullifyingKey();
  const masterPublicKey = WalletNode.getMasterPublicKey(spending.pubkey, nullifyingKey);
  return {
    seed,
    index,
    spendingPrivateKey: hex(spending.privateKey),
    spendingPublicKey: spending.pubkey.map(word),
    viewingPrivateKey: hex(viewing.privateKey),
    viewingPublicKey: hex(viewing.pubkey),
    nullifyingKey: word(nullifyingKey),
    masterPublicKey: word(masterPublicKey),
    address: encodeAddress({ masterPublicKey, viewingPublicKey: viewing.pubkey }),
  };
}

/** The `random` a shield note hides from everyone but the holder of `viewingPrivateKey`. */
async function decryptRandom(viewingPrivateKey, encryptedBundle, shieldKey) {
  const sharedKey = await getSharedSymmetricKey(bytes(viewingPrivateKey), bytes(shieldKey));
  return ShieldNote.decryptRandom(encryptedBundle, sharedKey);
}

module.exports = {
  wallet,
  Babyjubjub,
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
};
