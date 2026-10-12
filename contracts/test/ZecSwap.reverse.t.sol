// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ZecSwap} from "../src/ZecSwap.sol";
import {Token} from "../src/Token.sol";
import {ZecSwapRailgunTest} from "./ZecSwap.railgun.t.sol";

contract ZecSwapReverseTest is ZecSwapRailgunTest {
    function test_reverseRejectsShortTokenTransfersWithoutLeavingEscrow() public {
        ShortToken token = new ShortToken();
        ZecSwap.ReverseOpen memory request = reverseOpen();
        request.token = address(token);
        bytes memory signature = openSig(request);
        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.openReverse(request, signature);
        bytes32 id = swaps.reverseSwapId(auth, [e.x, e.y]);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.None));
        assertEq(token.balanceOf(address(swaps)), 0);
    }

    /// A maker that opens the forward swap a user verified as a reverse escrow instead, signing
    /// as its own escrowing user with that swap's shares and terms, would leave the user no claim
    /// after `t0`. The escrow takes an id of its own: the forward id the user reads stays empty.
    function test_reverseEscrowCannotStandInForAForwardSwap() public {
        (address evil, uint256 evilKey) = makeAddrAndKey("evil");
        address victim = makeAddr("victim");
        usdc.mint(evil, AMOUNT);
        vm.prank(evil);
        usdc.approve(address(swaps), AMOUNT);
        ZecSwap.ReverseOpen memory request = reverseOpen();
        (request.maker, request.user) = (victim, evil);
        (request.makerKey, request.userKey) = ([z.x, z.y], [e.x, e.y]);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(evilKey, digestOf(openHash(request)));
        vm.prank(evil);
        bytes32 id = swaps.openReverse(request, abi.encodePacked(r, s, v));

        ZecSwap.Terms memory forward =
            ZecSwap.Terms(evil, address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], victim, t0, t1, bytes32(0));
        assertEq(swaps.getSwap(id).termsHash, swaps.hashTerms(forward));
        assertEq(id, swaps.reverseSwapId(evil, [z.x, z.y]));
        assertEq(uint8(swaps.getSwap(swaps.swapId(evil, [z.x, z.y])).stage), uint8(ZecSwap.Stage.None));
    }

    /// A deposit of a token that delivers less than it was asked for credits nothing.
    function test_depositRejectsShortTokenTransfers() public {
        ShortToken token = new ShortToken();
        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.deposit(address(token), AMOUNT);
        assertEq(swaps.balanceOf(address(this), address(token)), 0);
    }

    /// More than a Railgun note holds could never leave after the claim, so it never opens.
    function test_openRefusesRailgunPayoutsANoteCannotHold() public {
        uint128 amount = uint128(type(uint120).max) + 1;
        usdc.mint(maker, amount);
        vm.startPrank(maker);
        usdc.approve(address(swaps), amount);
        swaps.deposit(address(usdc), amount);
        vm.expectRevert(ZecSwap.InvalidAmount.selector);
        swaps.open(address(usdc), amount, [e.x, e.y], [z.x, z.y], auth, t0, t1, commitment());
        vm.stopPrank();
    }

    function test_reverseReadySignatureCannotAuthorizeRefundLock() public {
        bytes32 id = funded();
        uint64 deadline = uint64(block.timestamp + 60);
        bytes memory signature =
            sign(keccak256(abi.encode(keccak256("Ready(bytes32 id,uint64 deadline)"), id, deadline)));
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockRefundWithSig(id, reverseTerms(), deadline, signature);
        swaps.readyWithSig(id, reverseTerms(), deadline, signature);
    }

    function reverseOpen() internal view returns (ZecSwap.ReverseOpen memory) {
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

    /// What `openReverse` stores: the escrowing user is the swap's maker, the ZEC side its user,
    /// each with the other's share.
    function reverseTerms() internal view returns (ZecSwap.Terms memory) {
        return ZecSwap.Terms(auth, address(usdc), AMOUNT, [z.x, z.y], [e.x, e.y], maker, t0, t1, bytes32(0));
    }

    function openSig(ZecSwap.ReverseOpen memory request) internal view returns (bytes memory) {
        return sign(openHash(request));
    }

    function openHash(ZecSwap.ReverseOpen memory request) internal pure returns (bytes32) {
        return keccak256(
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
        );
    }

    function funded() internal returns (bytes32) {
        ZecSwap.ReverseOpen memory request = reverseOpen();
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
            reverseTerms(),
            deadline,
            sign(keccak256(abi.encode(keccak256("Ready(bytes32 id,uint64 deadline)"), id, deadline)))
        );
    }

    function lockReverse(bytes32 id) internal {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        swaps.lockRefundWithSig(
            id,
            reverseTerms(),
            deadline,
            sign(keccak256(abi.encode(keccak256("LockRefund(bytes32 id,uint64 deadline)"), id, deadline)))
        );
    }

    function refundReverse(bytes32 id) internal {
        lockReverse(id);
        swaps.refund(id, reverseTerms(), z.k);
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
        assertEq(swaps.getSwap(id).termsHash, swaps.hashTerms(reverseTerms()));
        assertEq(swaps.balanceOf(auth, address(usdc)), 0);
        assertEq(usdc.balanceOf(relayer), 0);
        (bytes32 note, uint64 height) = swaps.reverseFunding(id);
        assertEq(note, commitment());
        assertEq(height, block.number);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.Unauthorized.selector);
        swaps.ready(id, reverseTerms());
        readyReverse(id);
        vm.prank(maker);
        swaps.lockClaim(id, reverseTerms());
        usdc.setPaused(true);
        swaps.claim(id, reverseTerms(), e.k);
        assertEq(swaps.getSwap(id).secret, e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY + AMOUNT);
    }

    function test_reverseOfflineUserCannotLoseFundsToTimeoutClaim() public {
        bytes32 id = funded();
        vm.warp(t1 + 30 days);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.lockClaim(id, reverseTerms());
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
        swaps.refundPayout(id, reverseTerms(), bytes32(uint256(7)), ciphertext, FEE, sig);
        railgun.setPaused(true);
        vm.prank(relayer);
        vm.expectRevert();
        swaps.refundPayout(id, reverseTerms(), npk, ciphertext, FEE, sig);
        assertFalse(swaps.getSwap(id).paidOut);
        railgun.setPaused(false);
        vm.prank(relayer);
        swaps.refundPayout(id, reverseTerms(), npk, ciphertext, FEE, sig);
        assertTrue(swaps.getSwap(id).paidOut);
        assertEq(railgun.last().npk, npk);
        assertEq(usdc.balanceOf(relayer), FEE);
        vm.prank(relayer);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.refundPayout(id, reverseTerms(), npk, ciphertext, FEE, sig);
    }

    function test_reverseOpenSignatureBindsAllTermsAndCannotReplay() public {
        ZecSwap.ReverseOpen memory request = reverseOpen();
        bytes memory sig = openSig(request);
        request.amount += 1;
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.openReverse(request, sig);
        request = reverseOpen();
        request.maker = address(0xbad);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.openReverse(request, sig);
        request = reverseOpen();
        request.refundNote = bytes32(uint256(123));
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.openReverse(request, sig);
        request = reverseOpen();
        vm.expectRevert(Token.TransferFailed.selector);
        swaps.openReverse(request, sig);
        bytes32 id = swaps.reverseSwapId(auth, [e.x, e.y]);
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
        swaps.lockClaim(id, reverseTerms());
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
        swaps.refundPayout(id, reverseTerms(), npk, ciphertext, FEE, refundPayoutSig(id));
        usdc.mint(swaps.vaultOf(id), AMOUNT);
        vm.prank(relayer);
        swaps.rescue(
            id,
            reverseTerms(),
            npk,
            ciphertext,
            FEE,
            0,
            uint64(block.timestamp + 5 minutes),
            rescueSig(id, npk, ciphertext, relayer, FEE)
        );
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
