// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Pallas} from "../src/Pallas.sol";
import {IRailgun} from "../src/ShieldVault.sol";
import {Token} from "../src/Token.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

contract ZecSwapTest is SpendAuthVectors {
    uint256 internal constant LOCK = 2 hours;
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
        swaps = new ZecSwap(LOCK, IRailgun(address(0)));
        usdc = new TestToken();
        usdc.mint(maker, INVENTORY);
        vm.startPrank(maker);
        usdc.approve(address(swaps), type(uint256).max);
        swaps.deposit(address(usdc), INVENTORY);
        vm.stopPrank();

        e = randomVector(0);
        z = randomVector(1);
        t0 = uint64(block.timestamp + 45 minutes);
        t1 = uint64(block.timestamp + 105 minutes);
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
        assertEq(s.claimLockUntil, 0);
        assertEq(s.refundLockUntil, 0);
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

    function test_open_rejectsDeadlinesOutOfOrder() public {
        vm.startPrank(maker);
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.open(
            address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, uint64(block.timestamp), t1, bytes32(0)
        );
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, t0, t0, bytes32(0));
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
        vm.startPrank(maker);
        swaps.lockRefund(id, terms());
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.ready(id, terms());
    }

    // claim

    function test_claim_creditsTheUserAndPublishesTheShare() public {
        bytes32 id = openReady();
        vm.prank(user);
        swaps.lockClaim(id, terms());

        vm.expectEmit(address(swaps));
        emit ZecSwap.Claimed(id, z.k);
        swaps.claim(id, terms(), z.k);

        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Claimed));
        assertEq(swaps.getSwap(id).secret, z.k);
    }

    function test_claim_settlesWhileTheTokenIsPausedAndPaysOutAfterwards() public {
        bytes32 id = openReady();
        vm.prank(user);
        swaps.lockClaim(id, terms());
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

    function test_claim_canBeSubmittedByAnyoneWhileTheUserHoldsTheLock() public {
        bytes32 id = openReady();
        vm.prank(user);
        swaps.lockClaim(id, terms());
        vm.prank(relayer);
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
        assertEq(swaps.balanceOf(relayer, address(usdc)), 0);
    }

    function test_claim_withoutReadyOpensAtT0() public {
        bytes32 id = open();
        vm.startPrank(user);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.lockClaim(id, terms());

        vm.warp(t0);
        swaps.lockClaim(id, terms());
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
    }

    function test_claim_neverExpires() public {
        bytes32 id = openReady();
        vm.warp(t1 + 365 days);
        vm.startPrank(user);
        swaps.lockClaim(id, terms());
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
    }

    function test_claim_requiresAHeldLock() public {
        bytes32 id = openReady();
        vm.expectRevert(ZecSwap.LockNotHeld.selector);
        swaps.claim(id, terms(), z.k);

        vm.prank(user);
        swaps.lockClaim(id, terms());
        vm.warp(block.timestamp + LOCK);
        vm.expectRevert(ZecSwap.LockNotHeld.selector);
        swaps.claim(id, terms(), z.k);
    }

    function test_claim_rejectsAnythingButTheUserShare() public {
        bytes32 id = openReady();
        vm.prank(user);
        swaps.lockClaim(id, terms());
        uint256[4] memory wrong = [e.k, z.k + 1, z.k + Pallas.Q, 0];
        for (uint256 i; i < wrong.length; ++i) {
            vm.expectRevert(ZecSwap.WrongSecret.selector);
            swaps.claim(id, terms(), wrong[i]);
        }
    }

    function test_lockClaim_isTheUsersAndWaitsOutTheMakersTurnAfterALapse() public {
        bytes32 id = openReady();
        vm.expectRevert(ZecSwap.Unauthorized.selector);
        vm.prank(maker);
        swaps.lockClaim(id, terms());

        vm.startPrank(user);
        swaps.lockClaim(id, terms());
        vm.warp(block.timestamp + LOCK);
        vm.expectRevert(ZecSwap.LockUnavailable.selector);
        swaps.lockClaim(id, terms());

        vm.warp(block.timestamp + LOCK);
        swaps.lockClaim(id, terms());
        swaps.claim(id, terms(), z.k);
    }

    function test_lockClaim_waitsOutAnActiveRefundLock() public {
        bytes32 id = open();
        vm.warp(t0);
        vm.prank(maker);
        swaps.lockRefund(id, terms());
        vm.expectRevert(ZecSwap.LockUnavailable.selector);
        vm.prank(user);
        swaps.lockClaim(id, terms());
    }

    // refund

    function test_refund_cancelsAnOpenSwapAndPublishesTheShare() public {
        bytes32 id = open();
        vm.prank(maker);
        swaps.lockRefund(id, terms());

        vm.expectEmit(address(swaps));
        emit ZecSwap.Refunded(id, e.k);
        swaps.refund(id, terms(), e.k);

        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Refunded));
        assertEq(swaps.getSwap(id).secret, e.k);
    }

    function test_refund_ofAReadySwapWaitsForT1() public {
        bytes32 id = openReady();
        vm.startPrank(maker);
        vm.warp(t1 - 1);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.lockRefund(id, terms());

        vm.warp(t1);
        swaps.lockRefund(id, terms());
        swaps.refund(id, terms(), e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY);
    }

    function test_lockRefund_isTheMakersAndWaitsOutTheUsersTurnAfterALapse() public {
        bytes32 id = open();
        vm.expectRevert(ZecSwap.Unauthorized.selector);
        vm.prank(user);
        swaps.lockRefund(id, terms());

        vm.startPrank(maker);
        swaps.lockRefund(id, terms());
        vm.warp(block.timestamp + LOCK);
        vm.expectRevert(ZecSwap.LockUnavailable.selector);
        swaps.lockRefund(id, terms());

        vm.warp(block.timestamp + LOCK);
        swaps.lockRefund(id, terms());
        swaps.refund(id, terms(), e.k);
    }

    /// The live run's abandoned-claim sequence: both sides let a lock lapse, and the swap still
    /// settles instead of stranding the funds.
    function test_locksLapsingOnBothSidesNeverStrandTheSwap() public {
        bytes32 id = openReady();
        vm.prank(user);
        swaps.lockClaim(id, terms());

        vm.warp(t1 + LOCK);
        vm.prank(maker);
        swaps.lockRefund(id, terms());
        vm.warp(block.timestamp + LOCK);

        vm.prank(maker);
        vm.expectRevert(ZecSwap.LockUnavailable.selector);
        swaps.lockRefund(id, terms());

        vm.warp(block.timestamp + LOCK);
        vm.startPrank(maker);
        swaps.lockRefund(id, terms());
        swaps.refund(id, terms(), e.k);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Refunded));
    }

    function test_lockRefund_waitsOutAnActiveClaimLock() public {
        bytes32 id = openReady();
        vm.warp(t1 - 1);
        vm.prank(user);
        swaps.lockClaim(id, terms());
        vm.warp(t1);
        vm.expectRevert(ZecSwap.LockUnavailable.selector);
        vm.prank(maker);
        swaps.lockRefund(id, terms());
    }

    function test_refund_requiresAHeldLockAndTheMakerShare() public {
        bytes32 id = open();
        vm.expectRevert(ZecSwap.LockNotHeld.selector);
        swaps.refund(id, terms(), e.k);

        vm.prank(maker);
        swaps.lockRefund(id, terms());
        vm.expectRevert(ZecSwap.WrongSecret.selector);
        swaps.refund(id, terms(), z.k);
    }

    // settlement is final

    function test_aClaimedSwapCannotBeRefunded() public {
        bytes32 id = openReady();
        vm.prank(user);
        swaps.lockClaim(id, terms());
        swaps.claim(id, terms(), z.k);

        vm.warp(t1 + LOCK);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        vm.prank(maker);
        swaps.lockRefund(id, terms());
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.refund(id, terms(), e.k);
    }

    function test_aRefundedSwapCannotBeClaimed() public {
        bytes32 id = open();
        vm.prank(maker);
        swaps.lockRefund(id, terms());
        swaps.refund(id, terms(), e.k);

        vm.warp(t1 + LOCK);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        vm.prank(user);
        swaps.lockClaim(id, terms());
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.claim(id, terms(), z.k);
    }

    // an expired lock hands the swap to the other side

    function test_anExpiredClaimLockLetsTheMakerRefund() public {
        bytes32 id = openReady();
        vm.warp(t1 - 1 minutes);
        vm.prank(user);
        swaps.lockClaim(id, terms());

        vm.warp(t1 - 1 minutes + LOCK);
        vm.startPrank(maker);
        swaps.lockRefund(id, terms());
        swaps.refund(id, terms(), e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY);
    }

    function test_anExpiredRefundLockLetsTheUserClaim() public {
        bytes32 id = open();
        vm.prank(maker);
        swaps.lockRefund(id, terms());

        vm.warp(t0 + LOCK);
        vm.startPrank(user);
        swaps.lockClaim(id, terms());
        swaps.claim(id, terms(), z.k);
        assertEq(swaps.balanceOf(user, address(usdc)), AMOUNT);
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

    function test_constructor_rejectsDegenerateLockDurations() public {
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        new ZecSwap(0, IRailgun(address(0)));
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        new ZecSwap(uint256(type(uint32).max) + 1, IRailgun(address(0)));
    }

    function test_gas_claim() public {
        bytes32 id = openReady();
        vm.prank(user);
        swaps.lockClaim(id, terms());
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
        return ZecSwap.Terms(maker, address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, t0, t1, bytes32(0));
    }
}
