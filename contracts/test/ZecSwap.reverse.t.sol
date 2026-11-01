// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {IRailgun} from "../src/ShieldVault.sol";
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
        // The id wallets compute: keccak256(abi.encode(userAuthAddress, makerShare, true)).
        assertEq(id, keccak256(abi.encode(evil, z.x, z.y, true)));
        assertEq(swaps.reverseSwapId(evil, [z.x, z.y]), id);
        assertEq(uint8(swaps.getSwap(swaps.swapId(evil, [z.x, z.y])).stage), uint8(ZecSwap.Stage.None));
    }

    /// A deposit of a token that delivers less than it was asked for credits nothing.
    function test_depositRejectsShortTokenTransfers() public {
        ShortToken token = new ShortToken();
        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.deposit(address(token), AMOUNT);
        assertEq(swaps.balanceOf(address(this), address(token)), 0);
    }

    /// More than a Railgun note holds could never leave after the claim, so it never opens; all a
    /// note holds does, and a swap that pays an account has no such cap.
    function test_openRefusesRailgunPayoutsANoteCannotHold() public {
        uint128 amount = uint128(type(uint120).max) + 1;
        usdc.mint(maker, 2 * uint256(amount));
        vm.startPrank(maker);
        swaps.deposit(address(usdc), 2 * uint256(amount));
        vm.expectRevert(ZecSwap.InvalidAmount.selector);
        swaps.open(address(usdc), amount, [e.x, e.y], [z.x, z.y], auth, t0, t1, commitment());
        swaps.open(address(usdc), amount - 1, [e.x, e.y], [z.x, z.y], auth, t0, t1, commitment());
        (Vector memory e2, Vector memory z2) = (randomVector(2), randomVector(3));
        swaps.open(address(usdc), amount, [e2.x, e2.y], [z2.x, z2.y], auth, t0, t1, bytes32(0));
        vm.stopPrank();
    }

    /// What arrives must be exactly what is credited: a token that delivers more than it was asked
    /// for is refused, as one that delivers less is.
    function test_tokensThatDeliverMoreAreRefused() public {
        LongToken token = new LongToken();
        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.deposit(address(token), AMOUNT);
        ZecSwap.ReverseOpen memory request = reverseOpen();
        request.token = address(token);
        bytes memory signature = openSig(request);
        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.openReverse(request, signature);
        assertEq(token.balanceOf(address(swaps)), 0);
    }

    /// The escrowing user's `Ready` signature authorises nothing else of its: not the payout of
    /// its refund, which it signs for separately.
    function test_reverseReadySignatureAuthorisesOnlyReady() public {
        bytes32 id = funded();
        bytes memory signature = readySig(id, uint64(block.timestamp + 60));
        refundReverse(id);
        vm.prank(relayer);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.refundPayout(id, reverseTerms(), npk, ciphertext, FEE, signature);
        vm.prank(relayer);
        swaps.refundPayout(id, reverseTerms(), npk, ciphertext, FEE, refundPayoutSig(id));
        assertTrue(swaps.getSwap(id).paidOut);
    }

    /// The escrow must leave its user `MIN_WINDOW` to sign ready before `t0`, as a forward open
    /// leaves its maker to cancel.
    function test_reverseOpenLeavesTheUserAWindowToReadyBeforeT0() public {
        ZecSwap.ReverseOpen memory request = reverseOpen();
        request.t0 = uint64(block.timestamp + WINDOW - 1);
        usdc.mint(relayer, AMOUNT);
        vm.startPrank(relayer);
        usdc.approve(address(swaps), AMOUNT);
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.openReverse(request, openSig(request));
        request.t0 = uint64(block.timestamp + WINDOW);
        swaps.openReverse(request, openSig(request));
        vm.stopPrank();
    }

    /// A request the escrow could not settle as its user signed it is refused before a token moves.
    function test_reverseOpenRefusesRequestsItCouldNotSettle() public {
        ZecSwap.ReverseOpen memory request = reverseOpen();
        request.deadline = uint64(block.timestamp - 1);
        assertRefused(swaps, request, ZecSwap.Expired.selector);

        request = reverseOpen();
        request.user = address(0);
        assertRefused(swaps, request, ZecSwap.ZeroAddress.selector);
        request = reverseOpen();
        request.maker = address(0);
        assertRefused(swaps, request, ZecSwap.ZeroAddress.selector);

        // With no refund note the escrow would read as a forward swap, which its ZEC maker may claim
        // from `t0` without the user's ready.
        request = reverseOpen();
        request.refundNote = 0;
        assertRefused(swaps, request, ZecSwap.WrongNote.selector);

        request = reverseOpen();
        request.amount = uint128(type(uint120).max) + 1;
        assertRefused(swaps, request, ZecSwap.InvalidAmount.selector);

        request = reverseOpen();
        request.deadline = t0;
        assertRefused(swaps, request, ZecSwap.InvalidDeadlines.selector);
        request.deadline = t0 + 1;
        assertRefused(swaps, request, ZecSwap.InvalidDeadlines.selector);
    }

    /// Without Railgun a reverse refund could never be paid out, so no escrow opens.
    function test_reverseOpenNeedsRailgun() public {
        ZecSwap noRailgun = new ZecSwap(WINDOW, LATE, IRailgun(address(0)));
        assertRefused(noRailgun, reverseOpen(), ZecSwap.NoShieldedPayouts.selector);
    }

    /// Each limit is the last value allowed: a request sent the second its signature expires, for
    /// all a Railgun note holds, opens, and so does one signed to expire the second before `t0`.
    function test_reverseOpenAcceptsEachLimitExactly() public {
        ZecSwap.ReverseOpen memory request = reverseOpen();
        request.deadline = uint64(block.timestamp);
        request.amount = type(uint120).max;
        bytes32 id = fundedWith(request);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Open));
        assertEq(usdc.balanceOf(address(swaps)), INVENTORY + type(uint120).max);

        request = reverseOpen();
        (Vector memory m, Vector memory u) = (randomVector(2), randomVector(3));
        (request.makerKey, request.userKey) = ([m.x, m.y], [u.x, u.y]);
        request.deadline = t0 - 1;
        id = fundedWith(request);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Open));
    }

    /// An escrow's open leaves whatever its user's address already holds here as it was.
    function test_reverseOpenLeavesItsUsersBalance() public {
        usdc.mint(auth, AMOUNT);
        vm.startPrank(auth);
        usdc.approve(address(swaps), AMOUNT);
        swaps.deposit(address(usdc), AMOUNT);
        vm.stopPrank();
        funded();
        assertEq(swaps.balanceOf(auth, address(usdc)), AMOUNT);
    }

    /// Only the escrowing user's signature readies its escrow. A ready the maker could make itself
    /// would let it claim the escrow before `t1` without ever sending the ZEC.
    function test_reverseReadyTakesOnlyTheUsersSignature() public {
        bytes32 id = funded();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        (, uint256 strangerKey) = makeAddrAndKey("stranger");
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(strangerKey, digestOf(readyHash(id, deadline)));
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.readyWithSig(id, reverseTerms(), deadline, abi.encodePacked(r, s, v));
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Open));
        vm.prank(maker);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, reverseTerms(), e.k);
    }

    /// A signed ready is a reverse escrow's alone: a forward swap is readied by its maker's own call,
    /// even when its maker signs. Each ready emits `MarkedReady`, from which the maker records it.
    function test_forwardSwapsTakeNoSignedReady() public {
        (address keyed, uint256 keyedKey) = makeAddrAndKey("keyed maker");
        usdc.mint(keyed, AMOUNT);
        vm.startPrank(keyed);
        usdc.approve(address(swaps), AMOUNT);
        swaps.deposit(address(usdc), AMOUNT);
        bytes32 id = swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], auth, t0, t1, commitment());
        vm.stopPrank();
        ZecSwap.Terms memory forward = terms();
        forward.maker = keyed;
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(keyedKey, digestOf(readyHash(id, deadline)));
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.readyWithSig(id, forward, deadline, abi.encodePacked(r, s, v));
        vm.expectEmit(address(swaps));
        emit ZecSwap.MarkedReady(id);
        vm.prank(keyed);
        swaps.ready(id, forward);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Ready));
    }

    /// The user's ready signature is good until its deadline, that second included.
    function test_reverseReadySignatureLastsUntilItsDeadline() public {
        bytes32 id = funded();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory signature = readySig(id, deadline);
        vm.warp(deadline + 1);
        vm.expectRevert(ZecSwap.Expired.selector);
        swaps.readyWithSig(id, reverseTerms(), deadline, signature);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Open));

        deadline = uint64(block.timestamp);
        signature = readySig(id, deadline);
        vm.expectEmit(address(swaps));
        emit ZecSwap.MarkedReady(id);
        swaps.readyWithSig(id, reverseTerms(), deadline, signature);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Ready));
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
        return fundedWith(reverseOpen());
    }

    /// Relay Adapt's part: approve the exact amount and open.
    function fundedWith(ZecSwap.ReverseOpen memory request) internal returns (bytes32 id) {
        usdc.mint(relayer, request.amount);
        vm.startPrank(relayer);
        usdc.approve(address(swaps), request.amount);
        id = swaps.openReverse(request, openSig(request));
        vm.stopPrank();
    }

    /// Relay Adapt's open of `request`, signed by its user for `target`, refused with `error`, no
    /// escrow recorded and no token taken.
    function assertRefused(ZecSwap target, ZecSwap.ReverseOpen memory request, bytes4 error) internal {
        bytes memory signature = signFor(target, openHash(request));
        usdc.mint(relayer, request.amount);
        uint256 held = usdc.balanceOf(address(target));
        vm.startPrank(relayer);
        usdc.approve(address(target), request.amount);
        vm.expectRevert(error);
        target.openReverse(request, signature);
        vm.stopPrank();
        assertEq(usdc.balanceOf(address(target)), held);
        bytes32 id = target.reverseSwapId(request.user, request.makerKey);
        assertEq(uint8(target.getSwap(id).stage), uint8(ZecSwap.Stage.None));
    }

    function readyReverse(bytes32 id) internal {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        swaps.readyWithSig(id, reverseTerms(), deadline, readySig(id, deadline));
    }

    function readySig(bytes32 id, uint64 deadline) internal view returns (bytes memory) {
        return sign(readyHash(id, deadline));
    }

    function readyHash(bytes32 id, uint64 deadline) internal pure returns (bytes32) {
        return keccak256(abi.encode(keccak256("Ready(bytes32 id,uint64 deadline)"), id, deadline));
    }

    /// The user's refund, which anyone may send: an unready escrow's at any time, a ready one's
    /// from `t1`.
    function refundReverse(bytes32 id) internal {
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
        vm.prank(maker);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, reverseTerms(), e.k);
        readyReverse(id);
        usdc.setPaused(true);
        vm.prank(maker);
        swaps.claim(id, reverseTerms(), e.k);
        assertEq(swaps.getSwap(id).secret, e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY + AMOUNT);
    }

    function test_reverseOfflineUserCannotLoseFundsToTimeoutClaim() public {
        bytes32 id = funded();
        vm.warp(t1 + 30 days);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, reverseTerms(), e.k);
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
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, reverseTerms(), e.k);
    }

    /// A ready escrow is the maker's to claim until `t1` sharp, and the user's to refund from
    /// then, for good.
    function test_reverseReadyProtectsMakerUntilRefundDeadline() public {
        bytes32 id = funded();
        readyReverse(id);
        vm.warp(t1 - 1);
        vm.expectRevert(ZecSwap.NotNow.selector);
        refundReverse(id);
        vm.warp(t1);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, reverseTerms(), e.k);
        vm.warp(t1 + LATE + 30 days);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, reverseTerms(), e.k);
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

contract LongToken {
    mapping(address => uint256) public balanceOf;

    function transferFrom(address, address to, uint256 amount) external returns (bool) {
        balanceOf[to] += amount + 1;
        return true;
    }
}
