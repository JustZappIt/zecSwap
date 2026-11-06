// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {TxParam} from "@forshtat/hegota-support/contracts/HegotaSupport.sol";
import {ZecSwap} from "zecswap/ZecSwap.sol";
import {WindowsBase} from "zecswap-test/utils/WindowsBase.sol";
import {ClaimSponsor} from "../src/ClaimSponsor.sol";
import {FrameHelpers, MockApprove, MockRegistry, MockTxParam} from "./utils/FrameMocks.sol";

/// The sponsor's pay frame on real swaps of a local ZecSwap, with APPROVE and TXPARAM stood in for.
contract ClaimSponsorTest is WindowsBase {
    event Approved(uint256 scope);

    address internal constant ENTRY_POINT = 0x00000000000000000000000000000000000000AA;
    uint256 internal constant PAYMENT = 1;
    uint256 internal constant WINDOW = 30 minutes;
    uint256 internal constant MAX_COST = 0.005 ether;

    ZecSwap internal swaps;
    MockRegistry internal registry;
    ClaimSponsor internal sponsor;
    uint64 internal t0;
    uint64 internal t1;

    function setUp() public override {
        super.setUp();
        swaps = deploy(WINDOW, 2 * WINDOW);
        registry = new MockRegistry();
        sponsor = newSponsor(maker);
        t0 = uint64(block.timestamp + WINDOW);
        t1 = t0 + uint64(WINDOW);
    }

    function test_paysTheFirstTransactionOfAnOpenOrReadySwapsOwnKey() public {
        Case memory c = forward(0);
        assertApproves(sponsor, c);
        (bool ok,) = doReady(c, 0);
        assertTrue(ok);
        assertApproves(sponsor, c);
    }

    function test_afterwhichTheSwapsKeyClaimsAndWithdraws() public {
        Case memory c = forward(1);
        doReady(c, 0);
        assertApproves(sponsor, c);
        // The SENDER frames, which run as the swap's key.
        vm.prank(c.terms.user);
        swaps.claim(c.id, c.terms, c.claimShare);
        vm.prank(c.terms.user);
        swaps.withdraw(address(usdc), AMOUNT, c.terms.user);
        assertEq(usdc.balanceOf(c.terms.user), AMOUNT);
    }

    /// Neither the share nor the window: the client judges both before it reveals.
    function test_paysForClaimsItCannotJudge() public {
        Case memory c = forward(2);
        doReady(c, 0);
        assertApproves(sponsor, c);
        (, bytes4 sel) = doClaim(c, c.claimShare + 1);
        assertEq(sel, ZecSwap.WrongSecret.selector);
        vm.warp(t1);
        assertApproves(sponsor, c);
        (, sel) = doClaim(c, c.claimShare);
        assertEq(sel, ZecSwap.NotNow.selector);
    }

    function test_refusesTermsThatAreNotTheSwaps() public {
        Case memory c = forward(3);
        c.terms.amount -= 1;
        assertRefuses(sponsor, c, ClaimSponsor.WrongTerms.selector);
    }

    function test_refusesASwapThatDoesNotExist() public {
        Case memory c = forward(4);
        c.id = keccak256("no swap");
        assertRefuses(sponsor, c, ClaimSponsor.WrongTerms.selector);
    }

    function test_refusesAClaimedSwap() public {
        Case memory c = forward(5);
        doReady(c, 0);
        (bool ok,) = doClaim(c, c.claimShare);
        assertTrue(ok);
        assertRefuses(sponsor, c, ClaimSponsor.NotClaimable.selector);
    }

    function test_refusesARefundedSwap() public {
        Case memory c = forward(6);
        (bool ok,) = doRefund(c, c.refundShare);
        assertTrue(ok);
        assertRefuses(sponsor, c, ClaimSponsor.NotClaimable.selector);
    }

    function test_refusesAReverseEscrow() public {
        Case memory c = openReverse(swaps, makeAddr("zec maker"), 7, 8, t0, t1, uint64(block.timestamp + 1));
        // A sponsor of the escrowing user's, so that only the swap's direction is wrong.
        ClaimSponsor s = newSponsor(c.terms.maker);
        inTx(s, c.terms.user, 0, MAX_COST);
        assertRefuses(s, c, ClaimSponsor.NotSponsored.selector);
    }

    function test_refusesASwapPayingIntoRailgun() public {
        (, uint256[2] memory e) = pair(9);
        (, uint256[2] memory z) = pair(10);
        Case memory c;
        c.terms =
            ZecSwap.Terms(maker, address(usdc), AMOUNT, e, z, makeAddr("user"), t0, t1, keccak256("note"));
        vm.prank(maker);
        c.id = swaps.open(address(usdc), AMOUNT, e, z, c.terms.user, t0, t1, c.terms.payoutNote);
        inTx(sponsor, c.terms.user, 0, MAX_COST);
        assertRefuses(sponsor, c, ClaimSponsor.NotSponsored.selector);
    }

    function test_refusesAnotherMakersSwap() public {
        address other = makeAddr("other maker");
        fund(swaps, other, AMOUNT);
        Case memory c = openForward(swaps, other, 11, 12, t0, t1);
        inTx(sponsor, c.terms.user, 0, MAX_COST);
        assertRefuses(sponsor, c, ClaimSponsor.NotSponsored.selector);
    }

    function test_refusesAnyoneButTheSwapsKey() public {
        Case memory c = forward(13);
        inTx(sponsor, makeAddr("someone else"), 0, MAX_COST);
        assertRefuses(sponsor, c, ClaimSponsor.WrongSender.selector);
    }

    function test_refusesTheKeysLaterTransactions() public {
        Case memory c = forward(14);
        inTx(sponsor, c.terms.user, 1, MAX_COST);
        assertRefuses(sponsor, c, ClaimSponsor.NotFirst.selector);
    }

    function test_paysUpToItsCap() public {
        Case memory c = forward(15);
        inTx(sponsor, c.terms.user, 0, MAX_COST + 1);
        assertRefuses(sponsor, c, ClaimSponsor.TooCostly.selector);
        inTx(sponsor, c.terms.user, 0, MAX_COST);
        assertApproves(sponsor, c);
    }

    function test_answersOnlyTheEntryPoint() public {
        Case memory c = forward(16);
        vm.expectRevert(ClaimSponsor.NotEntryPoint.selector);
        vm.prank(c.terms.user);
        sponsor.sponsor(c.id, c.terms);
    }

    function test_stakesWhatItIsSentAndKeepsItsOwn() public {
        uint256 before = address(sponsor).balance;
        address anyone = makeAddr("anyone");
        vm.deal(anyone, 0.012 ether);
        vm.prank(anyone);
        sponsor.stake{value: 0.012 ether}(1 days);
        assertEq(registry.stakeOf(address(sponsor)), 0.012 ether);
        assertEq(registry.delayOf(address(sponsor)), 1 days);
        assertEq(address(sponsor).balance, before);
    }

    function test_takesPlainTransfers() public {
        uint256 before = address(sponsor).balance;
        (bool ok,) = payable(address(sponsor)).call{value: 1 ether}("");
        assertTrue(ok);
        assertEq(address(sponsor).balance, before + 1 ether);
    }

    /// No call moves its ETH: it leaves only as the gas of transactions it approves.
    function testFuzz_ethLeavesOnlyAsGas(address caller, uint96 value, bytes calldata data) public {
        vm.assume(caller != address(sponsor));
        Case memory c = forward(17);
        uint256 before = address(sponsor).balance;
        vm.deal(caller, value);
        vm.prank(caller);
        (bool ok,) = address(sponsor).call{value: value}(data);
        ok;
        assertGe(address(sponsor).balance, before);
        pay(sponsor, c);
        assertGe(address(sponsor).balance, before);
    }

    /// What the stand-ins assume: the devnet's seven helpers are the first contracts it creates.
    function test_createsTheDevnetsHelpersFirst() public {
        ClaimSponsor s = new ClaimSponsor(swaps, maker, registry, MAX_COST);
        assertEq(vm.getNonce(address(s)), FrameHelpers.NEXT_NONCE);
        for (uint256 n = 1; n < FrameHelpers.NEXT_NONCE; ++n) {
            assertGt(vm.computeCreateAddress(address(s), n).code.length, 0);
        }
    }

    function newSponsor(address maker_) internal returns (ClaimSponsor s) {
        s = new ClaimSponsor{value: 1 ether}(swaps, maker_, registry, MAX_COST);
        address approve = vm.computeCreateAddress(address(s), FrameHelpers.APPROVE_NONCE);
        address txparam = vm.computeCreateAddress(address(s), FrameHelpers.TXPARAM_NONCE);
        vm.etch(approve, address(new MockApprove()).code);
        vm.etch(txparam, address(new MockTxParam()).code);
    }

    /// A forward swap of `maker`'s, in a transaction its key sends first and under the cap.
    function forward(uint256 i) internal returns (Case memory c) {
        c = openForward(swaps, maker, 2 * i + 100, 2 * i + 101, t0, t1);
        inTx(sponsor, c.terms.user, 0, MAX_COST);
    }

    /// The transaction the pay frame runs in: `sender`'s at `nonce`, costing at most `cost`.
    function inTx(ClaimSponsor s, address sender, uint256 nonce, uint256 cost) internal {
        MockTxParam params = MockTxParam(vm.computeCreateAddress(address(s), FrameHelpers.TXPARAM_NONCE));
        params.set(uint256(TxParam.SENDER), uint160(sender));
        params.set(uint256(TxParam.NONCE), nonce);
        params.set(uint256(TxParam.MAX_COST), cost);
    }

    function pay(ClaimSponsor s, Case memory c) internal returns (bool ok, bytes4 sel) {
        bytes memory ret;
        vm.prank(ENTRY_POINT);
        (ok, ret) = address(s).call(abi.encodeCall(ClaimSponsor.sponsor, (c.id, c.terms)));
        sel = selectorOf(ret);
    }

    function assertApproves(ClaimSponsor s, Case memory c) internal {
        vm.expectEmit(address(s));
        emit Approved(PAYMENT);
        (bool ok,) = pay(s, c);
        assertTrue(ok);
    }

    function assertRefuses(ClaimSponsor s, Case memory c, bytes4 error) internal {
        (bool ok, bytes4 sel) = pay(s, c);
        assertFalse(ok);
        assertEq(sel, error);
    }
}
