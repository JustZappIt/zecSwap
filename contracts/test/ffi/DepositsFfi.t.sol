// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {Deposits, ISanctions} from "../../src/Deposits.sol";
import {IRailgun} from "../../src/ShieldVault.sol";
import {MockRailgun} from "../utils/MockRailgun.sol";
import {TestToken} from "../utils/TestToken.sol";

/// zecswap-core's deposit nonce, EIP-3009 digest and rescue digest against the contracts, on
/// random deposits: each must land signed over Rust's digests. See the `ffi` profile in
/// foundry.toml; wallets and relayers sign and check deposits with the Rust ones.
contract DepositsFfiTest is Test {
    uint256 internal constant SECP256K1_N = 0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141;

    struct Deposit {
        uint256 key;
        address from;
        uint256 value;
        uint128 fee;
        address relayer;
        bytes32 npk;
        IRailgun.ShieldCiphertext ciphertext;
        uint256 validBefore;
    }

    Deposits internal deposits;
    TestToken internal token;
    MockRailgun internal railgun;

    function setUp() public {
        vm.skip(keccak256(bytes(vm.envOr("FOUNDRY_PROFILE", string("")))) != keccak256("ffi"));
        token = new TestToken();
        railgun = new MockRailgun();
        deposits = new Deposits(address(token), railgun, ISanctions(address(0)));
    }

    function testFuzz_aShieldAndARescueSignedOverRustsDigestsLand(
        uint256 key,
        uint96 value,
        uint96 fee,
        address relayer,
        bytes32 npk,
        bytes32[3] memory bundle,
        bytes32 shieldKey,
        uint64 chainId
    ) public {
        Deposit memory d;
        d.key = bound(key, 1, SECP256K1_N - 1);
        d.from = vm.addr(d.key);
        d.value = bound(value, 2, 1e15);
        d.fee = uint128(bound(fee, 0, d.value - 1));
        d.relayer = relayer;
        d.npk = npk;
        d.ciphertext = IRailgun.ShieldCiphertext(bundle, shieldKey);
        d.validBefore = block.timestamp + 1 hours;
        vm.assume(
            relayer != address(0) && relayer != d.from && relayer != address(deposits)
                && relayer != address(token) && relayer != address(railgun) && relayer != deposits.vaultOf(d.from)
        );
        vm.chainId(bound(chainId, 1, type(uint64).max));
        token.mint(d.from, d.value);

        bytes32 nonce = rustNonce(d);
        assertEq(nonce, deposits.depositNonce(npk, d.ciphertext, d.fee, relayer));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(d.key, rustReceiveDigest(d, nonce));
        vm.prank(relayer);
        deposits.shield(d.from, d.value, 0, d.validBefore, npk, d.ciphertext, d.fee, v, r, s);

        token.mint(deposits.vaultOf(d.from), d.value);
        bytes memory signature = sign(d.key, rustRescueDigest(d));
        vm.prank(relayer);
        deposits.rescue(d.from, npk, d.ciphertext, d.fee, 0, uint64(d.validBefore), signature);
        assertEq(token.balanceOf(relayer), 2 * uint256(d.fee));
    }

    function rustNonce(Deposit memory d) internal returns (bytes32) {
        string[] memory cmd = new string[](11);
        cmd[0] = binary();
        cmd[1] = "deposit-nonce";
        cmd[2] = vm.toString(block.chainid);
        cmd[3] = vm.toString(address(deposits));
        cmd[4] = vm.toString(d.npk);
        cmd[5] = vm.toString(d.ciphertext.encryptedBundle[0]);
        cmd[6] = vm.toString(d.ciphertext.encryptedBundle[1]);
        cmd[7] = vm.toString(d.ciphertext.encryptedBundle[2]);
        cmd[8] = vm.toString(d.ciphertext.shieldKey);
        cmd[9] = vm.toString(d.fee);
        cmd[10] = vm.toString(d.relayer);
        return abi.decode(vm.ffi(cmd), (bytes32));
    }

    function rustReceiveDigest(Deposit memory d, bytes32 nonce) internal returns (bytes32) {
        string[] memory cmd = new string[](12);
        cmd[0] = binary();
        cmd[1] = "receive-digest";
        cmd[2] = token.name();
        cmd[3] = token.version();
        cmd[4] = vm.toString(block.chainid);
        cmd[5] = vm.toString(address(token));
        cmd[6] = vm.toString(d.from);
        cmd[7] = vm.toString(address(deposits));
        cmd[8] = vm.toString(d.value);
        cmd[9] = "0";
        cmd[10] = vm.toString(d.validBefore);
        cmd[11] = vm.toString(nonce);
        return abi.decode(vm.ffi(cmd), (bytes32));
    }

    function rustRescueDigest(Deposit memory d) internal returns (bytes32) {
        string[] memory cmd = new string[](10);
        cmd[0] = binary();
        cmd[1] = "deposit-rescue";
        cmd[2] = vm.toString(block.chainid);
        cmd[3] = vm.toString(address(deposits));
        cmd[4] = vm.toString(d.from);
        cmd[5] = vm.toString(deposits.noteCommitment(d.npk, d.ciphertext));
        cmd[6] = vm.toString(d.relayer);
        cmd[7] = vm.toString(d.fee);
        cmd[8] = "0";
        cmd[9] = vm.toString(d.validBefore);
        return abi.decode(vm.ffi(cmd), (bytes32));
    }

    function sign(uint256 key, bytes32 digest) internal pure returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    function binary() internal view returns (string memory) {
        return string.concat(vm.projectRoot(), "/../target/release/examples/evm_vectors");
    }
}
