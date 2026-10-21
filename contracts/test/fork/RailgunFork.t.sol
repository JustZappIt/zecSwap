// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {console} from "forge-std/Test.sol";

import {IRailgun} from "../../src/ShieldVault.sol";
import {IERC20} from "../../src/Token.sol";
import {ZecSwap} from "../../src/ZecSwap.sol";
import {SpendAuthVectors} from "../utils/SpendAuthVectors.sol";
import {IRailgunTree, RailgunFixture} from "./RailgunFixture.sol";

/// Railgun payouts against the real Railgun and USDC on an Ethereum mainnet fork, paying the note
/// zecswap-railgun built in `vectors/railgun_note.json`. Skipped unless ETH_RPC_URL is set:
///   ETH_RPC_URL=… forge test --match-path test/fork/RailgunFork.t.sol -vv
contract RailgunForkTest is SpendAuthVectors, RailgunFixture {
    bytes32 internal constant PAYOUT_TYPEHASH = keccak256("Payout(bytes32 id,address relayer,uint128 fee)");
    bytes32 internal constant RESCUE_TYPEHASH =
        keccak256("Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)");
    uint128 internal constant AMOUNT = 500e6;
    uint128 internal constant FEE = 2e6;

    ZecSwap internal swaps;
    address internal maker = makeAddr("maker");
    address internal relayer = makeAddr("relayer");
    address internal auth;
    uint256 internal authKey;

    function setUp() public override {
        forkMainnet();
        super.setUp();

        swaps = new ZecSwap(1 hours, 24 hours, IRailgun(RAILGUN));
        deal(USDC, maker, AMOUNT);
        vm.startPrank(maker);
        IERC20(USDC).approve(address(swaps), AMOUNT);
        swaps.deposit(USDC, AMOUNT);
        vm.stopPrank();
        (auth, authKey) = makeAddrAndKey("auth");
    }

    function test_rustAndSolidityCommitToTheSameNote() public view {
        assertEq(swaps.noteCommitment(npk, ciphertext), commitment);
    }

    function test_aClaimedSwapShieldsIntoRailgun() public {
        (bytes32 id,, uint256 shieldGas) = claimAndPayOut();
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
        (bytes32 id, ZecSwap.Terms memory swapTerms,) = claimAndPayOut();
        address vault = swaps.vaultOf(id);
        deal(USDC, vault, 400e6);

        uint256 leaves = leafCount();
        bytes memory sig = sign(
            keccak256(
                abi.encode(
                    RESCUE_TYPEHASH,
                    id,
                    commitment,
                    relayer,
                    FEE,
                    uint64(0),
                    uint64(block.timestamp + 5 minutes)
                )
            )
        );
        vm.recordLogs();
        vm.prank(relayer);
        swaps.rescue(id, swapTerms, npk, ciphertext, FEE, 0, uint64(block.timestamp + 5 minutes), sig);
        (IRailgun.CommitmentPreimage[] memory notes,,) = shieldEvent();
        assertEq(notes[0].npk, npk);
        assertEq(leafCount(), leaves + 1);
        assertEq(IERC20(USDC).balanceOf(vault), 0);
    }

    function claimAndPayOut() internal returns (bytes32 id, ZecSwap.Terms memory t, uint256 payoutGas) {
        t = terms();
        vm.startPrank(maker);
        uint256 gas = gasleft();
        id = swaps.open(t.token, t.amount, t.makerKey, t.userKey, t.user, t.t0, t.t1, t.payoutNote);
        console.log("open gas", gas - gasleft());
        gas = gasleft();
        swaps.ready(id, t);
        console.log("ready gas", gas - gasleft());
        vm.stopPrank();

        uint256 userSecret = randomVector(1).k;
        vm.startPrank(relayer);
        gas = gasleft();
        swaps.claim(id, t, userSecret);
        console.log("claim gas", gas - gasleft());

        uint256 leaves = leafCount();
        bytes memory payoutSig = sign(keccak256(abi.encode(PAYOUT_TYPEHASH, id, relayer, FEE)));
        vm.recordLogs();
        gas = gasleft();
        swaps.payout(id, t, npk, ciphertext, FEE, payoutSig);
        payoutGas = gas - gasleft();
        vm.stopPrank();
        assertEq(leafCount(), leaves + 1);
    }

    /// A swap paying the note in `vectors/railgun_note.json`, between shares 0 and 1.
    function terms() internal view returns (ZecSwap.Terms memory) {
        Vector memory e = randomVector(0);
        Vector memory z = randomVector(1);
        uint64 t0 = uint64(block.timestamp + 90 minutes);
        return ZecSwap.Terms(maker, USDC, AMOUNT, [e.x, e.y], [z.x, z.y], auth, t0, t0 + 1 hours, commitment);
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
