// Run from engine/ to regenerate the Rust validator fixture with the installed V2 ABI.
// Dummy SNARK: tests encoding and authorization, not Railgun proving.
const fs = require('node:fs');
const { AbiCoder, Interface, Wallet, keccak256, zeroPadValue } = require('ethers');
const abi = require('./node_modules/@railgun-community/engine/dist/abi/V2/RelayAdapt.json');
const { RelayAdaptHelper } = require('./node_modules/@railgun-community/engine/dist/contracts/relay-adapt/relay-adapt-helper');
const points = require('../../../contracts/test/vectors/spend_auth_g.json').vectors;
const escrow = require('../../../contracts/out/ZecSwap.sol/ZecSwap.json').abi;
const coder = AbiCoder.defaultAbiCoder();
const relay = new Interface(abi);
const address = byte => '0x' + byte.repeat(20);
const word = byte => '0x' + byte.repeat(32);
(async () => {
  const chainId = 11155111;
  const contract = address('11'), relayer = address('22'), token = address('33'), maker = address('44'), to = address('55');
  const fee = 250000n;
  const user = new Wallet(word('07'));
  const terms = {
    maker, user: user.address, token, amount: 1000000n,
    makerKey: [points[2].x, points[2].y], userKey: [points[3].x, points[3].y],
    t0: 2000003600, t1: 2000007200, refundNote: word('66'), deadline: 2000000000,
  };
  const types = { OpenReverse: [
    ['maker','address'], ['user','address'], ['token','address'], ['amount','uint128'],
    ['makerKey','bytes32'], ['userKey','bytes32'], ['t0','uint64'], ['t1','uint64'],
    ['refundNote','bytes32'], ['deadline','uint64'],
  ].map(([name,type]) => ({name,type})) };
  const signature = await user.signTypedData(
    {name:'ZecSwap', version:'1', chainId, verifyingContract:contract}, types,
    {...terms, makerKey:keccak256(coder.encode(['uint256[2]'],[terms.makerKey])),
      userKey:keccak256(coder.encode(['uint256[2]'],[terms.userKey]))},
  );
  const tokenData = {tokenType:0, tokenAddress:token, tokenSubID:0};
  const calls = [
    {to:token, value:0, data:new Interface(['function approve(address,uint256)']).encodeFunctionData('approve',[contract,terms.amount])},
    {to:contract, value:0, data:new Interface(escrow).encodeFunctionData('openReverse',[terms,signature])},
    {to:token, value:0, data:new Interface(['function transfer(address,uint256)']).encodeFunctionData('transfer',[relayer,fee])},
    {to, value:0, data:relay.encodeFunctionData('shield',[[{
      preimage:{npk:word('77'),token:tokenData,value:0},
      ciphertext:{encryptedBundle:[word('88'),word('99'),word('aa')],shieldKey:word('bb')},
    }]])},
  ];
  const transactions = [{
    proof:{a:{x:1,y:2},b:{x:[3,4],y:[5,6]},c:{x:7,y:8}}, merkleRoot:word('01'),
    nullifiers:[word('02')], commitments:[word('03')],
    boundParams:{treeNumber:0,minGasPrice:1,unshield:1,chainID:chainId,adaptContract:to,
      adaptParams:word('00'),commitmentCiphertext:[]},
    unshieldPreimage:{npk:zeroPadValue(to,32),token:tokenData,value:terms.amount+fee},
  }];
  const random = 'dd'.repeat(31), minGasLimit = 1000000n;
  transactions[0].boundParams.adaptParams = RelayAdaptHelper.getRelayAdaptParams(transactions,random,true,calls,minGasLimit);
  const data = relay.encodeFunctionData('relay',[transactions,RelayAdaptHelper.getActionData(random,true,calls,minGasLimit)]);
  const swapId = keccak256(coder.encode(['address','uint256[2]','bool'],[user.address,terms.makerKey,true]));
  const fixture = {source:'@railgun-community/engine 9.8.0 V2 ABI; dummy SNARK',chainId,contract,relayer,token,maker,to,fee:fee.toString(),swapId,data};
  fs.writeFileSync('../../zecswap-chain/src/evm/funding/sdk-vector.json',JSON.stringify(fixture,null,2)+'\n');
})().catch(error => { console.error(error); process.exitCode=1; });
