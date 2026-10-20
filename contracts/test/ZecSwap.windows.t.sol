// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ZecSwap} from "../src/ZecSwap.sol";
import {MockRailgun} from "./utils/MockRailgun.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Smoke tests of the windows design: who may reveal when, in both directions.
contract ZecSwapWindowsTest is SpendAuthVectors {
    uint256 internal constant WINDOW = 1 hours;
    uint256 internal constant LATE = 24 hours;
    uint128 internal constant AMOUNT = 50e6;
    bytes32 internal constant OPEN_REVERSE_TYPEHASH = keccak256(
        "OpenReverse(address maker,address user,address token,uint128 amount,bytes32 makerKey,bytes32 userKey,uint64 t0,uint64 t1,bytes32 refundNote,uint64 deadline)"
    );
    bytes32 internal constant READY_TYPEHASH = keccak256("Ready(bytes32 id,uint64 deadline)");

    ZecSwap internal swaps;
    TestToken internal usdc;
    address internal maker = makeAddr("maker");
    address internal adapt = makeAddr("relay-adapt");
    uint256 internal next;

    struct Case {
        bytes32 id;
        ZecSwap.Terms terms;
        uint256 claimShare;
        uint256 refundShare;
        uint256 authKey;
    }

    function setUp() public override {
        super.setUp();
        swaps = new ZecSwap(WINDOW, LATE, new MockRailgun());
        usdc = new TestToken();
        usdc.mint(maker, 1_000 * uint256(AMOUNT));
        vm.startPrank(maker);
        usdc.approve(address(swaps), type(uint256).max);
        swaps.deposit(address(usdc), 1_000 * uint256(AMOUNT));
        vm.stopPrank();
    }

    // Forward

    function test_forward_aReadySwapIsTheUsersUntilT1() public {
        Case memory c = forward(90 minutes, 150 minutes);
        vm.prank(maker);
        swaps.ready(c.id, c.terms);
        for (uint256 t = block.timestamp; t < c.terms.t1; t += 7 minutes) {
            vm.warp(t);
            assertFalse(refunds(c), "no refund before t1");
        }
        vm.warp(c.terms.t1 - 1);
        assertTrue(claims(c));
    }

    function test_forward_anUnreadySwapIsTheMakersBeforeT0TheUsersUntilT1AndTheMakersFromT1() public {
        Case memory c = forward(90 minutes, 150 minutes);
        vm.warp(c.terms.t0 - 1);
        assertTrue(refunds(c), "the maker cancels before t0");
        assertFalse(claims(c), "no claim before t0 without ready");
        vm.warp(c.terms.t0);
        assertFalse(refunds(c), "no cancel from t0");
        assertTrue(claims(c), "the user's way out from t0");
        vm.warp(c.terms.t1 - 1);
        assertFalse(refunds(c));
        assertTrue(claims(c));
        vm.warp(c.terms.t1);
        assertFalse(claims(c), "no claim from t1");
        assertTrue(refunds(c), "the maker refunds from t1");
        vm.warp(c.terms.t1 + LATE - 1);
        assertFalse(claims(c));
        assertTrue(refunds(c), "the maker's window lasts LATE_WINDOW");
        vm.warp(c.terms.t1 + LATE);
        assertFalse(refunds(c), "no refund once the maker's window is over");
        assertTrue(claims(c), "a user the maker didn't refund claims again");
    }

    /// A user who missed t1 on a ready swap gets the late window too.
    function test_forward_aReadySwapNobodySettledIsTheUsersAgainAfterTheLateWindow() public {
        Case memory c = forward(90 minutes, 150 minutes);
        vm.prank(maker);
        swaps.ready(c.id, c.terms);
        vm.warp(c.terms.t1 + LATE - 1);
        assertFalse(claims(c));
        assertTrue(refunds(c));
        vm.warp(c.terms.t1 + LATE + 30 days);
        assertTrue(claims(c));
        assertFalse(refunds(c));
    }

    /// Reverse escrows have no late window: from t1 the refund is the user's for good.
    function test_reverse_hasNoLateWindow() public {
        Case memory c = reverse(2 hours, 3 hours);
        ready(c);
        vm.warp(c.terms.t1 + LATE + 30 days);
        assertFalse(claims(c));
        assertTrue(refunds(c));
    }

    function test_forward_readyIsGivenOnlyBeforeT0() public {
        Case memory c = forward(90 minutes, 150 minutes);
        vm.warp(c.terms.t0);
        vm.prank(maker);
        vm.expectRevert(ZecSwap.Expired.selector);
        swaps.ready(c.id, c.terms);
    }

    function test_forward_anOpenLeavesTheMakerAWindowBeforeT0AndTheUserOneBeforeT1() public {
        (uint256[2] memory e, uint256[2] memory z) = keys();
        uint64 now_ = uint64(block.timestamp);
        vm.startPrank(maker);
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.open(address(usdc), AMOUNT, e, z, maker, now_ + uint64(WINDOW) - 1, now_ + 3 hours, 0);
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.open(address(usdc), AMOUNT, e, z, maker, now_ + 1 hours, now_ + 1 hours + uint64(WINDOW) - 1, 0);
        swaps.open(address(usdc), AMOUNT, e, z, maker, now_ + uint64(WINDOW), now_ + uint64(2 * WINDOW), 0);
        vm.stopPrank();
    }

    // Reverse

    function test_reverse_anOpenLeavesTheUserAWindowToReadyBeforeT0() public {
        (ZecSwap.ReverseOpen memory r, bytes memory sig,) =
            reverseOpen(randomVector(next++), randomVector(next++), uint64(WINDOW) - 1, uint64(2 * WINDOW));
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        vm.prank(adapt);
        swaps.openReverse(r, sig);
        reverse(uint64(WINDOW), uint64(2 * WINDOW));
    }

    function test_reverse_aReadyEscrowIsTheMakersUntilT1AndTheUsersFrom() public {
        Case memory c = reverse(2 hours, 3 hours);
        ready(c);
        for (uint256 t = block.timestamp; t < c.terms.t1; t += 7 minutes) {
            vm.warp(t);
            assertFalse(refunds(c), "no refund of a ready escrow before t1");
        }
        vm.warp(c.terms.t1 - 1);
        assertTrue(claims(c));
        vm.warp(c.terms.t1);
        assertFalse(claims(c), "no claim from t1");
        assertTrue(refunds(c), "the user refunds from t1");
    }

    function test_reverse_anUnreadyEscrowIsTheUsersAtAnyTime() public {
        Case memory c = reverse(2 hours, 3 hours);
        for (uint256 t = block.timestamp; t < c.terms.t1 + 1 days; t += 37 minutes) {
            vm.warp(t);
            assertFalse(claims(c), "no claim without ready");
            assertTrue(refunds(c));
        }
    }

    // Never both, whatever the time, stage and direction.

    function testFuzz_neverBothRevealsAtOnce(bool reverse_, bool makeReady, uint32 readyAt, uint32 at)
        public
    {
        Case memory c = reverse_ ? reverse(2 hours, 3 hours) : forward(90 minutes, 150 minutes);
        uint256 opened = block.timestamp;
        if (makeReady) {
            vm.warp(opened + bound(readyAt, 0, 2 hours));
            if (reverse_) tryReady(c);
            else tryMakerReady(c);
        }
        vm.warp(opened + bound(at, 0, 9 days));
        assertFalse(claims(c) && refunds(c), "both reveals possible at once");
    }

    // Helpers: each try is undone, so a test can ask both.

    function claims(Case memory c) internal returns (bool ok) {
        uint256 snap = vm.snapshotState();
        try swaps.claim(c.id, c.terms, c.claimShare) {
            ok = true;
        } catch {}
        vm.revertToState(snap);
    }

    function refunds(Case memory c) internal returns (bool ok) {
        uint256 snap = vm.snapshotState();
        try swaps.refund(c.id, c.terms, c.refundShare) {
            ok = true;
        } catch {}
        vm.revertToState(snap);
    }

    function tryMakerReady(Case memory c) internal {
        vm.prank(maker);
        try swaps.ready(c.id, c.terms) {} catch {}
    }

    function tryReady(Case memory c) internal {
        uint64 deadline = uint64(block.timestamp + 2 minutes);
        bytes memory sig = sign(c.authKey, keccak256(abi.encode(READY_TYPEHASH, c.id, deadline)));
        try swaps.readyWithSig(c.id, c.terms, deadline, sig) {} catch {}
    }

    function ready(Case memory c) internal {
        uint64 deadline = uint64(block.timestamp + 2 minutes);
        bytes memory sig = sign(c.authKey, keccak256(abi.encode(READY_TYPEHASH, c.id, deadline)));
        swaps.readyWithSig(c.id, c.terms, deadline, sig);
    }

    function keys() internal returns (uint256[2] memory e, uint256[2] memory z) {
        Vector memory a = randomVector(next++);
        Vector memory b = randomVector(next++);
        e = [a.x, a.y];
        z = [b.x, b.y];
    }

    function forward(uint64 t0After, uint64 t1After) internal returns (Case memory c) {
        Vector memory e = randomVector(next++);
        Vector memory z = randomVector(next++);
        address user = makeAddr(string.concat("user", vm.toString(next)));
        uint64 t0 = uint64(block.timestamp) + t0After;
        uint64 t1 = uint64(block.timestamp) + t1After;
        c.terms =
            ZecSwap.Terms(maker, address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, t0, t1, bytes32(0));
        vm.prank(maker);
        c.id = swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], user, t0, t1, bytes32(0));
        c.claimShare = z.k;
        c.refundShare = e.k;
    }

    /// Stored roles: the escrowing user is `maker`, the maker is `user`.
    function reverse(uint64 t0After, uint64 t1After) internal returns (Case memory c) {
        Vector memory m = randomVector(next++);
        Vector memory u = randomVector(next++);
        ZecSwap.ReverseOpen memory r;
        bytes memory sig;
        (r, sig, c.authKey) = reverseOpen(m, u, t0After, t1After);
        vm.prank(adapt);
        c.id = swaps.openReverse(r, sig);
        c.terms = ZecSwap.Terms(
            r.user, address(usdc), AMOUNT, [u.x, u.y], [m.x, m.y], maker, r.t0, r.t1, bytes32(0)
        );
        c.claimShare = m.k;
        c.refundShare = u.k;
    }

    /// A reverse open as Relay Adapt sends it, signed by a fresh per-swap key and funded.
    function reverseOpen(Vector memory m, Vector memory u, uint64 t0After, uint64 t1After)
        internal
        returns (ZecSwap.ReverseOpen memory r, bytes memory sig, uint256 authKey)
    {
        address auth;
        (auth, authKey) = makeAddrAndKey(string.concat("auth", vm.toString(next)));
        r = ZecSwap.ReverseOpen(
            maker,
            auth,
            address(usdc),
            AMOUNT,
            [m.x, m.y],
            [u.x, u.y],
            uint64(block.timestamp) + t0After,
            uint64(block.timestamp) + t1After,
            keccak256("refund note"),
            uint64(block.timestamp) + 10 minutes
        );
        sig = sign(
            authKey,
            keccak256(
                abi.encode(
                    OPEN_REVERSE_TYPEHASH,
                    r.maker,
                    r.user,
                    r.token,
                    r.amount,
                    keccak256(abi.encode(r.makerKey)),
                    keccak256(abi.encode(r.userKey)),
                    r.t0,
                    r.t1,
                    r.refundNote,
                    r.deadline
                )
            )
        );
        usdc.mint(adapt, AMOUNT);
        vm.prank(adapt);
        usdc.approve(address(swaps), AMOUNT);
    }

    function sign(uint256 key, bytes32 structHash) internal view returns (bytes memory) {
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
            vm.sign(key, keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, s, v);
    }
}
