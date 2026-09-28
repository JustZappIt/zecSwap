// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ZecSwap} from "../src/ZecSwap.sol";
import {Token} from "../src/Token.sol";
import {ZecSwapRailgunTest} from "./ZecSwap.railgun.t.sol";

contract ZecSwapReverseTest is ZecSwapRailgunTest {
    function test_reverseRejectsShortTokenTransfersWithoutLeavingEscrow() public {
        ShortToken token = new ShortToken();
        ZecSwap.ReverseOpen memory request = terms();
        request.token = address(token);
        bytes memory signature = openSig(request);
        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.openReverse(request, signature);
        bytes32 id = swaps.swapId(auth, [e.x, e.y]);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.None));
        assertEq(token.balanceOf(address(swaps)), 0);
    }

    function test_reverseReadySignatureCannotAuthorizeRefundLock() public {
        bytes32 id = funded();
        uint64 deadline = uint64(block.timestamp + 60);
        bytes memory signature =
            sign(keccak256(abi.encode(keccak256("Ready(bytes32 id,uint64 deadline)"), id, deadline)));
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockRefundWithSig(id, deadline, signature);
        swaps.readyWithSig(id, deadline, signature);
    }

    function terms() internal view returns (ZecSwap.ReverseOpen memory) {
        return ZecSwap.ReverseOpen(
            maker,
            auth,
            address(usdc),
            AMOUNT,
            [e.x, e.y],
            [z.x, z.y],
            t0,
            t1,
            commitment(),
            uint64(block.timestamp + 10 minutes)
        );
    }

    function openSig(ZecSwap.ReverseOpen memory request) internal view returns (bytes memory) {
        return sign(
            keccak256(
                abi.encode(
                    keccak256(
                        "OpenReverse(address maker,address user,address token,uint128 amount,bytes32 makerKey,bytes32 userKey,uint64 t0,uint64 t1,bytes32 refundNote,uint64 deadline)"
                    ),
                    request.maker,
                    request.user,
                    request.token,
                    request.amount,
                    keccak256(abi.encode(request.makerKey)),
                    keccak256(abi.encode(request.userKey)),
                    request.t0,
                    request.t1,
                    request.refundNote,
                    request.deadline
                )
            )
        );
    }

    function funded() internal returns (bytes32) {
        ZecSwap.ReverseOpen memory request = terms();
        usdc.mint(relayer, AMOUNT);
        vm.startPrank(relayer);
        usdc.approve(address(swaps), AMOUNT);
        bytes32 id = swaps.openReverse(request, openSig(request));
        vm.stopPrank();
        return id;
    }

    function readyReverse(bytes32 id) internal {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        swaps.readyWithSig(
            id,
            deadline,
            sign(keccak256(abi.encode(keccak256("Ready(bytes32 id,uint64 deadline)"), id, deadline)))
        );
    }

    function lockReverse(bytes32 id) internal {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        swaps.lockRefundWithSig(
            id,
            deadline,
            sign(keccak256(abi.encode(keccak256("LockRefund(bytes32 id,uint64 deadline)"), id, deadline)))
        );
    }

    function refundReverse(bytes32 id) internal {
        lockReverse(id);
        swaps.refund(id, z.k);
    }

    function refundPayoutSig(bytes32 id) internal view returns (bytes memory) {
        return sign(
            keccak256(
                abi.encode(
                    keccak256("RefundPayout(bytes32 id,address relayer,uint128 fee)"), id, relayer, FEE
                )
            )
        );
    }

    function test_reverseFundingIsAtomicAndRolesAreReversed() public {
        bytes32 id = funded();
        ZecSwap.Swap memory swap = swaps.getSwap(id);
        assertEq(swap.maker, auth);
        assertEq(swap.user, maker);
        assertEq(swap.makerX, z.x);
        assertEq(swap.userX, e.x);
        assertEq(swaps.balanceOf(auth, address(usdc)), 0);
        assertEq(usdc.balanceOf(relayer), 0);
        (bytes32 note, uint64 height) = swaps.reverseFunding(id);
        assertEq(note, commitment());
        assertEq(height, block.number);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.Unauthorized.selector);
        swaps.ready(id);
        readyReverse(id);
        vm.prank(maker);
        swaps.lockClaim(id);
        usdc.setPaused(true);
        swaps.claim(id, e.k);
        assertEq(swaps.getSwap(id).secret, e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY + AMOUNT);
    }

    function test_reverseOfflineUserCannotLoseFundsToTimeoutClaim() public {
        bytes32 id = funded();
        vm.warp(t1 + 30 days);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.lockClaim(id);
        vm.expectRevert(ZecSwap.Expired.selector);
        readyReverse(id);
        refundReverse(id);
        assertEq(swaps.getSwap(id).secret, z.k);
    }

    function test_reverseRefundCanOnlyShieldToCommittedNoteAndRetriesAfterPause() public {
        bytes32 id = funded();
        usdc.setPaused(true);
        refundReverse(id);
        assertEq(swaps.balanceOf(auth, address(usdc)), 0);
        usdc.setPaused(false);
        bytes memory sig = refundPayoutSig(id);
        vm.prank(relayer);
        vm.expectRevert(ZecSwap.WrongNote.selector);
        swaps.refundPayout(id, bytes32(uint256(7)), ciphertext, FEE, sig);
        railgun.setPaused(true);
        vm.prank(relayer);
        vm.expectRevert();
        swaps.refundPayout(id, npk, ciphertext, FEE, sig);
        assertFalse(swaps.getSwap(id).paidOut);
        railgun.setPaused(false);
        vm.prank(relayer);
        swaps.refundPayout(id, npk, ciphertext, FEE, sig);
        assertTrue(swaps.getSwap(id).paidOut);
        assertEq(railgun.last().npk, npk);
        assertEq(usdc.balanceOf(relayer), FEE);
        vm.prank(relayer);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.refundPayout(id, npk, ciphertext, FEE, sig);
    }

    function test_reverseOpenSignatureBindsAllTermsAndCannotReplay() public {
        ZecSwap.ReverseOpen memory request = terms();
        bytes memory sig = openSig(request);
        request.amount += 1;
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.openReverse(request, sig);
        request = terms();
        request.maker = address(0xbad);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.openReverse(request, sig);
        request = terms();
        request.refundNote = bytes32(uint256(123));
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.openReverse(request, sig);
        request = terms();
        vm.expectRevert(Token.TransferFailed.selector);
        swaps.openReverse(request, sig);
        bytes32 id = swaps.swapId(auth, [e.x, e.y]);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.None));
        funded();
        vm.expectRevert(ZecSwap.KeyReused.selector);
        swaps.openReverse(request, sig);
    }

    function test_reverseCancellationPreventsDelayedReadyAndRevealRace() public {
        bytes32 id = funded();
        refundReverse(id);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        readyReverse(id);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.lockClaim(id);
    }

    function test_reverseReadyProtectsMakerUntilRefundDeadline() public {
        bytes32 id = funded();
        readyReverse(id);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        lockReverse(id);
        vm.warp(t1);
        refundReverse(id);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Refunded));
    }

    function test_reverseReturnedRefundRescueBelongsToUser() public {
        bytes32 id = funded();
        refundReverse(id);
        vm.prank(relayer);
        swaps.refundPayout(id, npk, ciphertext, FEE, refundPayoutSig(id));
        usdc.mint(swaps.vaultOf(id), AMOUNT);
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, rescueSig(id, npk, ciphertext, relayer, FEE));
        assertEq(railgun.count(), 2);
    }
}

contract ShortToken {
    mapping(address => uint256) public balanceOf;

    function transferFrom(address, address to, uint256 amount) external returns (bool) {
        balanceOf[to] += amount - 1;
        return true;
    }
}
