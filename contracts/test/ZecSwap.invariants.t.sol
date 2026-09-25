// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {ZecSwap} from "../src/ZecSwap.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Drives random interleavings of every swap action and records any departure from the spec.
contract SwapHandler is Test {
    /// Few enough that random picks keep landing on swaps with a live lock.
    uint256 internal constant MAX_SWAPS = 4;

    struct Secrets {
        uint256 maker;
        uint256 user;
    }

    ZecSwap public immutable swaps;
    TestToken public immutable usdc;
    address public immutable maker = makeAddr("maker");
    address public immutable user = makeAddr("user");

    SpendAuthVectors.Vector[] internal keys;
    uint256 internal nextKey;

    bytes32[] public ids;
    mapping(bytes32 => Secrets) internal secrets;
    mapping(bytes32 => ZecSwap.Stage) internal settledAs;

    bool public settlementChanged;
    bool public revealUnderLockFailed;
    bool public revealWithoutLockSucceeded;
    bool public claimableSwapRefusedLock;
    uint256 public lockedAmount;

    constructor(ZecSwap swaps_, TestToken usdc_, SpendAuthVectors.Vector[] memory keys_) {
        swaps = swaps_;
        usdc = usdc_;
        for (uint256 i; i < keys_.length; ++i) {
            keys.push(keys_[i]);
        }
        usdc.mint(maker, type(uint128).max);
        vm.startPrank(maker);
        usdc.approve(address(swaps), type(uint256).max);
        swaps.deposit(address(usdc), type(uint128).max);
        vm.stopPrank();
    }

    function open(uint128 amount, uint32 t0Delay, uint32 window) external {
        if (ids.length == MAX_SWAPS) return;
        SpendAuthVectors.Vector memory e = keys[nextKey++];
        SpendAuthVectors.Vector memory z = keys[nextKey++];
        amount = uint128(bound(amount, 1, 1e12));
        uint64 t0 = uint64(block.timestamp + bound(t0Delay, 1, 2 hours));
        uint64 t1 = uint64(t0 + bound(window, 1, 4 hours));

        vm.prank(maker);
        bytes32 id = swaps.open(address(usdc), amount, [e.x, e.y], [z.x, z.y], user, t0, t1);
        ids.push(id);
        secrets[id] = Secrets(e.k, z.k);
        lockedAmount += amount;
    }

    function ready(uint256 i) external {
        (bytes32 id,) = pick(i);
        if (id == 0) return;
        vm.prank(maker);
        try swaps.ready(id) {} catch {}
        observe(id);
    }

    function lockClaim(uint256 i) external {
        (bytes32 id, ZecSwap.Swap memory s) = pick(i);
        if (id == 0) return;
        bool claimable =
            s.stage == ZecSwap.Stage.Ready || (s.stage == ZecSwap.Stage.Open && block.timestamp >= s.t0);
        bool locked = block.timestamp < s.claimLockUntil || block.timestamp < s.refundLockUntil;
        bool makersTurn =
            s.claimLockUntil > s.refundLockUntil && block.timestamp < s.claimLockUntil + swaps.LOCK_DURATION();
        bool available = !locked && !makersTurn;
        vm.prank(user);
        try swaps.lockClaim(id) {}
        catch {
            // Claims never expire: once claimable, only a held lock or the maker's turn refuses it.
            if (claimable && available) claimableSwapRefusedLock = true;
        }
        observe(id);
    }

    function lockRefund(uint256 i) external {
        (bytes32 id,) = pick(i);
        if (id == 0) return;
        vm.prank(maker);
        try swaps.lockRefund(id) {} catch {}
        observe(id);
    }

    function claim(uint256 i, bool honest) external {
        (bytes32 id, ZecSwap.Swap memory s) = pick(i);
        if (id == 0) return;
        bool held = unsettled(s) && block.timestamp < s.claimLockUntil;
        uint256 secret = honest ? secrets[id].user : secrets[id].maker;
        try swaps.claim(id, secret) {
            if (!held || !honest) revealWithoutLockSucceeded = true;
            lockedAmount -= s.amount;
        } catch {
            if (held && honest) revealUnderLockFailed = true;
        }
        observe(id);
    }

    function refund(uint256 i, bool honest) external {
        (bytes32 id, ZecSwap.Swap memory s) = pick(i);
        if (id == 0) return;
        bool held = unsettled(s) && block.timestamp < s.refundLockUntil;
        uint256 secret = honest ? secrets[id].maker : secrets[id].user;
        try swaps.refund(id, secret) {
            if (!held || !honest) revealWithoutLockSucceeded = true;
            lockedAmount -= s.amount;
        } catch {
            if (held && honest) revealUnderLockFailed = true;
        }
        observe(id);
    }

    function warp(uint32 by) external {
        vm.warp(block.timestamp + bound(by, 1, 90 minutes));
    }

    function secretsOf(bytes32 id) external view returns (uint256 maker, uint256 user) {
        return (secrets[id].maker, secrets[id].user);
    }

    function swapCount() external view returns (uint256) {
        return ids.length;
    }

    function pick(uint256 i) internal view returns (bytes32 id, ZecSwap.Swap memory s) {
        if (ids.length == 0) return (0, s);
        id = ids[i % ids.length];
        s = swaps.getSwap(id);
    }

    function observe(bytes32 id) internal {
        ZecSwap.Stage stage = swaps.getSwap(id).stage;
        ZecSwap.Stage settled = settledAs[id];
        if (settled != ZecSwap.Stage.None && stage != settled) settlementChanged = true;
        if (settled == ZecSwap.Stage.None && !unsettled(swaps.getSwap(id))) settledAs[id] = stage;
    }

    function unsettled(ZecSwap.Swap memory s) internal pure returns (bool) {
        return s.stage == ZecSwap.Stage.Open || s.stage == ZecSwap.Stage.Ready;
    }
}

contract ZecSwapInvariantTest is SpendAuthVectors {
    ZecSwap internal swaps;
    TestToken internal usdc;
    SwapHandler internal handler;

    function setUp() public override {
        super.setUp();
        swaps = new ZecSwap(2 hours);
        usdc = new TestToken();
        Vector[] memory keys = new Vector[](RANDOM_COUNT);
        for (uint256 i; i < RANDOM_COUNT; ++i) {
            keys[i] = randomVector(i);
        }
        handler = new SwapHandler(swaps, usdc, keys);
        targetContract(address(handler));
    }

    function invariant_settlementIsFinal() public view {
        assertFalse(handler.settlementChanged());
    }

    function invariant_aRevealUnderAHeldLockNeverFails() public view {
        assertFalse(handler.revealUnderLockFailed());
    }

    function invariant_noRevealSucceedsWithoutItsLockAndSecret() public view {
        assertFalse(handler.revealWithoutLockSucceeded());
    }

    function invariant_claimsNeverExpire() public view {
        assertFalse(handler.claimableSwapRefusedLock());
    }

    /// An open swap has revealed nothing; a settled one holds exactly its winner's share.
    function invariant_theSecretIsTheWinnersShare() public view {
        for (uint256 i; i < handler.swapCount(); ++i) {
            bytes32 id = handler.ids(i);
            ZecSwap.Swap memory s = swaps.getSwap(id);
            (uint256 makerSecret, uint256 userSecret) = handler.secretsOf(id);
            if (s.stage == ZecSwap.Stage.Claimed) assertEq(s.secret, userSecret);
            else if (s.stage == ZecSwap.Stage.Refunded) assertEq(s.secret, makerSecret);
            else assertEq(s.secret, 0);
        }
    }

    function invariant_atMostOneLockIsHeld() public view {
        for (uint256 i; i < handler.swapCount(); ++i) {
            ZecSwap.Swap memory s = swaps.getSwap(handler.ids(i));
            assertFalse(block.timestamp < s.claimLockUntil && block.timestamp < s.refundLockUntil);
        }
    }

    function invariant_tokensAreConserved() public view {
        uint256 inventory = swaps.balanceOf(handler.maker(), address(usdc));
        uint256 claimed = swaps.balanceOf(handler.user(), address(usdc));
        assertEq(usdc.balanceOf(address(swaps)), inventory + claimed + handler.lockedAmount());
        assertEq(usdc.balanceOf(address(swaps)), type(uint128).max);
    }
}
