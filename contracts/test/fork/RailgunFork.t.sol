// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Vm, console} from "forge-std/Test.sol";

import {IRailgun} from "../../src/ShieldVault.sol";
import {IERC20} from "../../src/Token.sol";
import {ZecSwap} from "../../src/ZecSwap.sol";
import {SpendAuthVectors} from "../utils/SpendAuthVectors.sol";

interface IRailgunTree {
    function treeNumber() external view returns (uint256);
    function nextLeafIndex() external view returns (uint256);
    function shieldFee() external view returns (uint120);
}

/// Railgun payouts against the real Railgun and USDC on an Ethereum mainnet fork, paying the note
/// zecswap-railgun built in `vectors/railgun_note.json`. Skipped unless ETH_RPC_URL is set:
///   ETH_RPC_URL=… forge test --match-path test/fork/RailgunFork.t.sol -vv
contract RailgunForkTest is SpendAuthVectors {
    address internal constant RAILGUN = 0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9;
    address internal constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    /// 2026-09-25; pinned so forks are cached and repeatable.
    uint256 internal constant FORK_BLOCK = 26_056_542;
    bytes32 internal constant SHIELD_EVENT = keccak256(
        "Shield(uint256,uint256,(bytes32,(uint8,address,uint256),uint120)[],(bytes32[3],bytes32)[],uint256[])"
    );
    bytes32 internal constant LOCK_CLAIM_TYPEHASH = keccak256("LockClaim(bytes32 id,uint64 deadline)");
    bytes32 internal constant PAYOUT_TYPEHASH = keccak256("Payout(bytes32 id,address relayer,uint128 fee)");
    bytes32 internal constant RESCUE_TYPEHASH =
        keccak256("Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)");
    uint256 internal constant LOCK = 2 hours;
    uint128 internal constant AMOUNT = 500e6;
    uint128 internal constant FEE = 2e6;

    ZecSwap internal swaps;
    address internal maker = makeAddr("maker");
    address internal relayer = makeAddr("relayer");
    address internal auth;
    uint256 internal authKey;
    bytes32 internal npk;
    IRailgun.ShieldCiphertext internal ciphertext;
    bytes32 internal commitment;

    function setUp() public override {
        string memory rpc = vm.envOr("ETH_RPC_URL", string(""));
        vm.skip(bytes(rpc).length == 0);
        super.setUp();
        vm.createSelectFork(rpc, FORK_BLOCK);

        swaps = new ZecSwap(LOCK, IRailgun(RAILGUN));
        deal(USDC, maker, AMOUNT);
        vm.startPrank(maker);
        IERC20(USDC).approve(address(swaps), AMOUNT);
        swaps.deposit(USDC, AMOUNT);
        vm.stopPrank();
        (auth, authKey) = makeAddrAndKey("auth");

        string memory json = vm.readFile(string.concat(vm.projectRoot(), "/test/vectors/railgun_note.json"));
        npk = vm.parseJsonBytes32(json, ".npk");
        bytes32[] memory bundle = vm.parseJsonBytes32Array(json, ".encryptedBundle");
        ciphertext.encryptedBundle = [bundle[0], bundle[1], bundle[2]];
        ciphertext.shieldKey = vm.parseJsonBytes32(json, ".shieldKey");
        commitment = vm.parseJsonBytes32(json, ".commitment");
    }

    function test_rustAndSolidityCommitToTheSameNote() public view {
        assertEq(swaps.noteCommitment(npk, ciphertext), commitment);
    }

    function test_aClaimedSwapShieldsIntoRailgun() public {
        (bytes32 id, uint256 shieldGas) = claimAndPayOut();
        uint120 value = uint120(AMOUNT - FEE);
        uint120 railgunFee = value * IRailgunTree(RAILGUN).shieldFee() / 10_000;

        (
            IRailgun.CommitmentPreimage[] memory notes,
            IRailgun.ShieldCiphertext[] memory ciphertexts,
            uint256[] memory fees
        ) = shieldEvent();
        assertEq(notes.length, 1);
        assertEq(notes[0].npk, npk);
        assertEq(notes[0].token.tokenType, 0);
        assertEq(notes[0].token.tokenAddress, USDC);
        assertEq(notes[0].value, value - railgunFee);
        assertEq(keccak256(abi.encode(ciphertexts[0])), keccak256(abi.encode(ciphertext)));
        assertEq(fees[0], railgunFee);
        assertEq(IERC20(USDC).balanceOf(relayer), FEE);
        assertEq(IERC20(USDC).balanceOf(address(swaps)), 0);
        assertEq(IERC20(USDC).balanceOf(swaps.vaultOf(id)), 0);
        console.log("payout gas", shieldGas);
    }

    function test_rescueShieldsWhatCameBackAgain() public {
        (bytes32 id,) = claimAndPayOut();
        address vault = swaps.vaultOf(id);
        deal(USDC, vault, 400e6);

        uint256 leaves = leafCount();
        bytes memory sig = sign(keccak256(abi.encode(RESCUE_TYPEHASH, id, commitment, relayer, FEE, uint64(0), uint64(block.timestamp + 5 minutes))));
        vm.recordLogs();
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 0, uint64(block.timestamp + 5 minutes), sig);
        (IRailgun.CommitmentPreimage[] memory notes,,) = shieldEvent();
        assertEq(notes[0].npk, npk);
        assertEq(leafCount(), leaves + 1);
        assertEq(IERC20(USDC).balanceOf(vault), 0);
    }

    function claimAndPayOut() internal returns (bytes32 id, uint256 payoutGas) {
        Vector memory e = randomVector(0);
        Vector memory z = randomVector(1);
        uint64 t0 = uint64(block.timestamp + 45 minutes);

        vm.startPrank(maker);
        uint256 gas = gasleft();
        id = swaps.open(USDC, AMOUNT, [e.x, e.y], [z.x, z.y], auth, t0, t0 + 1 hours, commitment);
        console.log("open gas", gas - gasleft());
        swaps.ready(id);
        vm.stopPrank();

        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory lockSig = sign(keccak256(abi.encode(LOCK_CLAIM_TYPEHASH, id, deadline)));
        vm.startPrank(relayer);
        gas = gasleft();
        swaps.lockClaimWithSig(id, deadline, lockSig);
        console.log("lockClaimWithSig gas", gas - gasleft());
        gas = gasleft();
        swaps.claim(id, z.k);
        console.log("claim gas", gas - gasleft());

        uint256 leaves = leafCount();
        bytes memory payoutSig = sign(keccak256(abi.encode(PAYOUT_TYPEHASH, id, relayer, FEE)));
        vm.recordLogs();
        gas = gasleft();
        swaps.payout(id, npk, ciphertext, FEE, payoutSig);
        payoutGas = gas - gasleft();
        vm.stopPrank();
        assertEq(leafCount(), leaves + 1);
    }

    /// The one `Shield` Railgun emitted since `vm.recordLogs`.
    function shieldEvent()
        internal
        view
        returns (
            IRailgun.CommitmentPreimage[] memory,
            IRailgun.ShieldCiphertext[] memory,
            uint256[] memory fees
        )
    {
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == RAILGUN && logs[i].topics[0] == SHIELD_EVENT) {
                (
                    ,
                    ,
                    IRailgun.CommitmentPreimage[] memory notes,
                    IRailgun.ShieldCiphertext[] memory ciphertexts,
                    uint256[] memory f
                ) = abi.decode(
                    logs[i].data,
                    (uint256, uint256, IRailgun.CommitmentPreimage[], IRailgun.ShieldCiphertext[], uint256[])
                );
                return (notes, ciphertexts, f);
            }
        }
        revert("no Shield event");
    }

    function leafCount() internal view returns (uint256) {
        return IRailgunTree(RAILGUN).treeNumber() * 2 ** 16 + IRailgunTree(RAILGUN).nextLeafIndex();
    }

    function sign(bytes32 structHash) internal view returns (bytes memory) {
        bytes32 domain = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
                ),
                keccak256("ZecSwap"),
                keccak256("1"),
                block.chainid,
                address(swaps)
            )
        );
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(authKey, keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, s, v);
    }
}
