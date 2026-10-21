// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Pallas} from "../src/Pallas.sol";
import {IRailgun} from "../src/ShieldVault.sol";
import {Token} from "../src/Token.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

contract ZecSwapTest is SpendAuthVectors {
    uint256 internal constant WINDOW = 1 hours;
    uint256 internal constant LATE = 24 hours;
    uint128 internal constant AMOUNT = 150e6;
    uint256 internal constant INVENTORY = 1_000e6;

    ZecSwap internal swaps;
    TestToken internal usdc;
    address internal maker = makeAddr("maker");
    address internal user = makeAddr("user");
    address internal relayer = makeAddr("relayer");

    Vector internal e;
    Vector internal z;
    uint64 internal t0;
    uint64 internal t1;

    function setUp() public override {
        super.setUp();
        swaps = new ZecSwap(WINDOW, LATE, IRailgun(address(0)));
        usdc = new TestToken();
        usdc.mint(maker, INVENTORY);
        vm.startPrank(maker);
        usdc.approve(address(swaps), type(uint256).max);
        swaps.deposit(address(usdc), INVENTORY);
        vm.stopPrank();

        e = randomVector(0);
        z = randomVector(1);
        t0 = uint64(block.timestamp + 90 minutes);
        t1 = uint64(block.timestamp + 150 minutes);
    }

    // open

    function test_open_recordsTheSwapAndDebitsInventory() public {
        bytes32 id = open();
        ZecSwap.Swap memory s = swaps.getSwap(id);
        assertEq(uint8(s.stage), uint8(ZecSwap.Stage.Open));
        // The definition wallets and relayers reproduce: the terms' ABI encoding, hashed.
        assertEq(s.termsHash, keccak256(abi.encode(terms())));
        assertEq(swaps.hashTerms(terms()), s.termsHash);
        assertFalse(s.paidOut);
        assertEq(s.secret, 0);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY - AMOUNT);
    }

    /// The cost cut this layout exists for: open stores the terms' hash and the stage, spends
    /// the maker's share and debits the inventory, and writes nothing else.
    function test_open_storesOnlyTheTermsHashAndStage() public {
        vm.record();
        bytes32 id = open();
        (, bytes32[] memory writes) = vm.accesses(address(swaps));
        bytes32 swapSlot = keccak256(abi.encode(id, uint256(2)));
        bytes32[4] memory expected = [
            keccak256(abi.encode(keccak256(abi.encode(maker, [e.x, e.y])), uint256(3))),
            keccak256(abi.encode(address(usdc), keccak256(abi.encode(maker, uint256(4))))),
            swapSlot,
            bytes32(uint256(swapSlot) + 1)
        ];
        assertEq(writes.length, expected.length);
        for (uint256 i; i < expected.length; ++i) {
            assertEq(writes[i], expected[i]);
        }
        assertEq(vm.load(address(swaps), swapSlot), keccak256(abi.encode(terms())));
    }

    function test_open_keysTheSwapByItsMakerAndTheUserShare() public {
        assertEq(open(), keccak256(abi.encode(maker, z.x, z.y)));
        assertEq(swaps.swapId(maker, [z.x, z.y]), keccak256(abi.encode(maker, z.x, z.y)));
    }

    /// On a public mempool anyone can copy a pending `open`, but the copy is its own swap: the
    /// maker's still opens, under the id its user expects.
    function test_open_underCopiedSharesCannotBlockTheMaker() public {
        address copier = makeAddr("copier");
        TestToken junk = new TestToken();
        junk.mint(copier, 1);
        vm.startPrank(copier);
        junk.approve(address(swaps), 1);
        swaps.deposit(address(junk), 1);
        bytes32 copy = swaps.open(address(junk), 1, [e.x, e.y], [z.x, z.y], copier, t0, t1, bytes32(0));
        vm.stopPrank();

        bytes32 id = open();
        assertTrue(copy != id);
        assertEq(swaps.getSwap(id).termsHash, swaps.hashTerms(terms()));
    }

    function test_open_emitsTheSharesForTheUserToVerify() public {
        vm.expectEmit(address(swaps));
        emit ZecSwap.Opened(
            keccak256(abi.encode(maker, z.x, z.y)),
            maker,
            user,
            address(usdc),
            AMOUNT,
            [e.x, e.y],
            [z.x, z.y],
            t0,
            t1,
            bytes32(0)
        );
        open();
    }

    function test_open_rejectsZeroAmountAndZeroUser() public {
        vm.startPrank(maker);
        vm.expectRevert(ZecSwap.ZeroAmount.selector);
        swaps.open(address(usdc), 0, [e.x, e.y], [z.x, z.y], user, t0, t1, bytes32(0));
        vm.expectRevert(ZecSwap.ZeroAddress.selector);
        swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], address(0), t0, t1, bytes32(0));
    }

    /// An open leaves the maker `MIN_WINDOW` to cancel before `t0`, and the user as much to
    /// claim before `t1`.
    function test_open_rejectsWindowsUnderTheMinimum() public {
        uint64 now_ = uint64(block.timestamp);
        vm.startPrank(maker);
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.open(
            address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, now_ + uint64(WINDOW) - 1, t1, bytes32(0)
        );
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.open(
            address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, t0, t0 + uint64(WINDOW) - 1, bytes32(0)
        );
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, t0, t0, bytes32(0));
        swaps.open(
            address(usdc),
            AMOUNT,
            [e.x, e.y],
            [z.x, z.y],
            user,
            now_ + uint64(WINDOW),
            now_ + uint64(2 * WINDOW),
            bytes32(0)
        );
    }

    function test_open_rejectsInvalidAndDegenerateKeys() public {
        uint256[2][4] memory badMakerKeys =
            [[e.x, e.y + 1], [uint256(0), uint256(0)], [z.x, z.y], [z.x, Pallas.P - z.y]];
        vm.startPrank(maker);
        for (uint256 i; i < badMakerKeys.length; ++i) {
            vm.expectRevert(ZecSwap.InvalidKey.selector);
            swaps.open(address(usdc), AMOUNT, badMakerKeys[i], [z.x, z.y], user, t0, t1, bytes32(0));
        }
        vm.expectRevert(ZecSwap.InvalidKey.selector);
        swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x + Pallas.P, z.y], user, t0, t1, bytes32(0));
    }

    function test_open_neverReusesAShare() public {
        open();
        Vector memory fresh = randomVector(2);
        vm.expectRevert(ZecSwap.KeyReused.selector);
        openWith(fresh, z);
        vm.expectRevert(ZecSwap.KeyReused.selector);
        openWith(e, fresh);
    }

    function test_open_rejectsARailgunPayoutWhereThereIsNoRailgun() public {
        vm.prank(maker);
        vm.expectRevert(ZecSwap.NoShieldedPayouts.selector);
        swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, t0, t1, keccak256("note"));
    }

    function test_open_rejectsMoreThanTheInventory() public {
        vm.prank(maker);
        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.open(address(usdc), uint128(INVENTORY + 1), [e.x, e.y], [z.x, z.y], user, t0, t1, bytes32(0));
    }

    // ready

    function test_ready_isTheMakersAttestationOnly() public {
        bytes32 id = open();
        vm.expectRevert(ZecSwap.Unauthorized.selector);
        vm.prank(user);
        swaps.ready(id, terms());

        vm.prank(maker);
        swaps.ready(id, terms());
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Ready));

        vm.expectRevert(ZecSwap.WrongStage.selector);
        vm.prank(maker);
        swaps.ready(id, terms());
    }

    function test_ready_cannotUndoACancellation() public {
        bytes32 id = open();
        swaps.refund(id, terms(), e.k);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        vm.prank(maker);
        swaps.ready(id, terms());
    }

    /// From `t0` an unready swap is the user's to claim anyway: a ready then would only move
    /// the maker's refund of a swap nobody claimed from `t0` to `t1`.
    function test_ready_isGivenOnlyBeforeT0() public {
        bytes32 id = open();
        vm.warp(t0);
        vm.expectRevert(ZecSwap.Expired.selector);
        vm.prank(maker);
        swaps.ready(id, terms());
    }

    // claim

    function test_claim_creditsTheUserAndPublishesTheShare() public {
        bytes32 id = openReady();
        vm.expectEmit(address(swaps));
        emit ZecSwap.Claimed(id, z.k);
        swaps.claim(id, terms(), z.k);

        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Claimed));
        assertEq(swaps.getSwap(id).secret, z.k);
    }

    function test_claim_settlesWhileTheTokenIsPausedAndPaysOutAfterwards() public {
        bytes32 id = openReady();
        usdc.setPaused(true);
        swaps.claim(id, terms(), z.k);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Claimed));

        vm.startPrank(user);
        vm.expectRevert(Token.TransferFailed.selector);
        swaps.withdraw(address(usdc), AMOUNT, user);
        usdc.setPaused(false);
        swaps.withdraw(address(usdc), AMOUNT, user);
        assertEq(usdc.balanceOf(user), AMOUNT);
        assertEq(swaps.balanceOf(user, address(usdc)), 0);
    }

    function test_claim_canBeSubmittedByAnyoneInTheUsersWindow() public {
        bytes32 id = openReady();
        vm.prank(relayer);
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
        assertEq(swaps.balanceOf(relayer, address(usdc)), 0);
    }

    function test_claim_withoutReadyOpensAtT0() public {
        bytes32 id = open();
        vm.warp(t0 - 1);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, terms(), z.k);

        vm.warp(t0);
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
    }

    /// The claim ends at `t1`, where the maker's refund begins; a swap the maker never refunds
    /// in `LATE_WINDOW` is the user's to claim again, for good.
    function test_claim_endsAtT1AndReturnsAfterTheLateWindow() public {
        bytes32 id = openReady();
        vm.warp(t1 - 1);
        assertTrue(claims(id));
        vm.warp(t1);
        assertFalse(claims(id), "no claim from t1");
        assertTrue(refunds(id), "the maker refunds from t1");
        vm.warp(t1 + LATE - 1);
        assertFalse(claims(id));
        assertTrue(refunds(id), "the maker's window lasts LATE_WINDOW");
        vm.warp(t1 + LATE);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.refund(id, terms(), e.k);
        vm.warp(t1 + LATE + 365 days);
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
    }

    function test_claim_outsideItsWindowReverts() public {
        bytes32 id = open();
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, terms(), z.k);
        vm.warp(t0 - 1);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, terms(), z.k);

        vm.prank(maker);
        swaps.ready(id, terms());
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
    }

    function test_claim_rejectsAnythingButTheUserShare() public {
        bytes32 id = openReady();
        uint256[4] memory wrong = [e.k, z.k + 1, z.k + Pallas.Q, 0];
        for (uint256 i; i < wrong.length; ++i) {
            vm.expectRevert(ZecSwap.WrongSecret.selector);
            swaps.claim(id, terms(), wrong[i]);
        }
    }

    // refund

    function test_refund_cancelsAnOpenSwapAndPublishesTheShare() public {
        bytes32 id = open();
        vm.expectEmit(address(swaps));
        emit ZecSwap.Refunded(id, e.k);
        swaps.refund(id, terms(), e.k);

        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Refunded));
        assertEq(swaps.getSwap(id).secret, e.k);
    }

    function test_refund_ofAReadySwapWaitsForT1() public {
        bytes32 id = openReady();
        vm.warp(t1 - 1);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.refund(id, terms(), e.k);

        vm.warp(t1);
        swaps.refund(id, terms(), e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY);
    }

    /// The cancel of an unready swap ends at `t0` sharp: from there the swap is the user's,
    /// deposit or not, until `t1`.
    function test_refund_ofAnUnreadySwapEndsAtT0() public {
        bytes32 id = open();
        vm.warp(t0 - 1);
        assertTrue(refunds(id));
        vm.warp(t0);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.refund(id, terms(), e.k);
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
    }

    function test_refund_canBeSubmittedByAnyoneWithTheMakerShare() public {
        bytes32 id = open();
        vm.expectRevert(ZecSwap.WrongSecret.selector);
        swaps.refund(id, terms(), z.k);
        vm.prank(relayer);
        swaps.refund(id, terms(), e.k);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Refunded));
    }

    /// Replaces the lock design's lapsing turns: at every moment of a forward swap's life exactly
    /// one side may reveal, so nothing is ever stranded and nothing is ever raced.
    function test_exactlyOneSideMayRevealAtEveryMoment() public {
        bytes32 unready = open();
        for (uint256 t = block.timestamp; t < t1 + LATE + 2 hours; t += 11 minutes) {
            vm.warp(t);
            assertTrue(claims(unready) != refunds(unready), "unready: one side, and one only");
        }
        vm.warp(1);
        Vector memory e2 = randomVector(2);
        Vector memory z2 = randomVector(3);
        bytes32 ready = openWith(e2, z2);
        vm.prank(maker);
        swaps.ready(ready, termsWith(e2, z2));
        for (uint256 t = block.timestamp; t < t1 + LATE + 2 hours; t += 11 minutes) {
            vm.warp(t);
            bool claim = tryClaim(ready, termsWith(e2, z2), z2.k);
            bool refund = tryRefund(ready, termsWith(e2, z2), e2.k);
            assertTrue(claim != refund, "ready: one side, and one only");
        }
    }

    /// A claim not mined by `t1` leaves the swap to the maker's refund: the user takes its
    /// deposit back with the revealed share instead.
    function test_aClaimNotSentByT1LeavesTheRefundToTheMaker() public {
        bytes32 id = openReady();
        vm.warp(t1);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, terms(), z.k);
        swaps.refund(id, terms(), e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY);
    }

    /// A cancel not mined by `t0` leaves an unready swap to the user's claim.
    function test_aCancelNotSentByT0LeavesTheClaimToTheUser() public {
        bytes32 id = open();
        vm.warp(t0);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.refund(id, terms(), e.k);
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
    }

    // settlement is final

    function test_aClaimedSwapCannotBeRefunded() public {
        bytes32 id = openReady();
        swaps.claim(id, terms(), z.k);

        vm.warp(t1);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.refund(id, terms(), e.k);
        vm.warp(t1 + LATE);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, terms(), z.k);
    }

    function test_aRefundedSwapCannotBeClaimed() public {
        bytes32 id = open();
        swaps.refund(id, terms(), e.k);

        vm.warp(t0);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, terms(), z.k);
        vm.warp(t1 + LATE);
        vm.expectRevert(ZecSwap.NotNow.selector);
        swaps.claim(id, terms(), z.k);
    }

    // never both, whatever the time and stage

    function testFuzz_neverBothRevealsAtOnce(bool makeReady, uint32 readyAt, uint32 at) public {
        bytes32 id = open();
        uint256 opened = block.timestamp;
        if (makeReady) {
            vm.warp(opened + bound(readyAt, 0, 2 hours));
            vm.prank(maker);
            try swaps.ready(id, terms()) {} catch {}
        }
        vm.warp(opened + bound(at, 0, 9 days));
        assertFalse(claims(id) && refunds(id), "both reveals possible at once");
    }

    // inventory

    function test_withdraw() public {
        vm.startPrank(maker);
        swaps.withdraw(address(usdc), 400e6, relayer);
        assertEq(usdc.balanceOf(relayer), 400e6);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY - 400e6);

        vm.expectRevert(ZecSwap.InsufficientBalance.selector);
        swaps.withdraw(address(usdc), INVENTORY, maker);
        vm.expectRevert(ZecSwap.ZeroAddress.selector);
        swaps.withdraw(address(usdc), 1, address(0));
    }

    function test_deposit_rejectsZeroAndCodelessTokens() public {
        vm.startPrank(maker);
        vm.expectRevert(ZecSwap.ZeroAmount.selector);
        swaps.deposit(address(usdc), 0);
        vm.expectRevert(Token.TransferFailed.selector);
        swaps.deposit(makeAddr("not a token"), 1);
    }

    function test_constructor_rejectsDegenerateWindows() public {
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        new ZecSwap(0, LATE, IRailgun(address(0)));
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        new ZecSwap(WINDOW, WINDOW - 1, IRailgun(address(0)));
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        new ZecSwap(WINDOW, uint256(type(uint32).max) + 1, IRailgun(address(0)));
        ZecSwap tight = new ZecSwap(WINDOW, WINDOW, IRailgun(address(0)));
        assertEq(tight.MIN_WINDOW(), WINDOW);
        assertEq(tight.LATE_WINDOW(), WINDOW);
    }

    function test_gas_claim() public {
        bytes32 id = openReady();
        uint256 start = gasleft();
        swaps.claim(id, terms(), z.k);
        assertLt(start - gasleft(), 250_000);
    }

    function open() internal returns (bytes32) {
        return openWith(e, z);
    }

    function openReady() internal returns (bytes32 id) {
        id = open();
        vm.prank(maker);
        swaps.ready(id, terms());
    }

    function openWith(Vector memory makerShare, Vector memory userShare) internal returns (bytes32) {
        vm.prank(maker);
        return swaps.open(
            address(usdc),
            AMOUNT,
            [makerShare.x, makerShare.y],
            [userShare.x, userShare.y],
            user,
            t0,
            t1,
            bytes32(0)
        );
    }

    /// The terms `open` commits to, which every later call supplies.
    function terms() internal view returns (ZecSwap.Terms memory) {
        return termsWith(e, z);
    }

    function termsWith(Vector memory makerShare, Vector memory userShare)
        internal
        view
        returns (ZecSwap.Terms memory)
    {
        return ZecSwap.Terms(
            maker,
            address(usdc),
            AMOUNT,
            [makerShare.x, makerShare.y],
            [userShare.x, userShare.y],
            user,
            t0,
            t1,
            bytes32(0)
        );
    }

    /// Whether the claim would succeed now; the state is left as it was.
    function claims(bytes32 id) internal returns (bool) {
        return tryClaim(id, terms(), z.k);
    }

    function refunds(bytes32 id) internal returns (bool) {
        return tryRefund(id, terms(), e.k);
    }

    function tryClaim(bytes32 id, ZecSwap.Terms memory t, uint256 share) internal returns (bool ok) {
        uint256 snap = vm.snapshotState();
        try swaps.claim(id, t, share) {
            ok = true;
        } catch {}
        vm.revertToState(snap);
    }

    function tryRefund(bytes32 id, ZecSwap.Terms memory t, uint256 share) internal returns (bool ok) {
        uint256 snap = vm.snapshotState();
        try swaps.refund(id, t, share) {
            ok = true;
        } catch {}
        vm.revertToState(snap);
    }
}
