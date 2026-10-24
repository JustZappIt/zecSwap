// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {Pallas} from "../src/Pallas.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {ZecSwapLocks} from "./deployed/ZecSwapLocks.sol";
import {WindowsBase} from "./utils/WindowsBase.sol";

/// Running the lock contract (as deployed on mainnet) and the windows contract side by side: what
/// a client of one does against the other. A windows-rules client against the lock contract
/// reveals without a lock and loses both legs; a lock-rules client against the windows contract
/// fails before it reveals, by accident of the ABI. The swap record itself changes shape: six
/// words to four. Every client must therefore check which contract it talks to (audit 3, A-2).
contract ZecSwapMigrationTest is WindowsBase {
    ZecSwap internal windows;
    ZecSwapLocks internal locks;
    uint256 internal constant START = 1_792_000_000;

    /// A reveal that is broadcast puts its share on this ledger, mined or reverted: the calldata
    /// is public either way.
    mapping(uint256 share => bool) internal publicShare;

    function setUp() public override {
        super.setUp();
        windows = deploy(1 hours, 24 hours);
        locks = new ZecSwapLocks(30 minutes, railgun);
        fund(ZecSwap(address(locks)), maker, 100 * uint256(AMOUNT));
        vm.warp(START);
    }

    /// A client built for windows ("from t0 I may claim an unready swap") pointed at the deployed
    /// contract: its claim reverts LockNotHeld with z public, and the lock contract's maker locks
    /// the refund (an Open swap is refundable at any time there), refunds, and sweeps with e + z.
    function test_windowsClientOnTheLockContractLosesBothLegs() public {
        (bytes32 id, ZecSwapLocks.Terms memory t, uint256 e, uint256 z) = lockSwap(41, 42);
        vm.warp(t.t0 + 1);
        (bool claimed, bytes4 sel) = land(address(locks), abi.encodeCall(ZecSwapLocks.claim, (id, t, z)));
        assertFalse(claimed);
        assertEq(sel, ZecSwapLocks.LockNotHeld.selector);
        assertTrue(publicShare[z], "z is public from the reverted claim");
        vm.prank(maker);
        locks.lockRefund(id, t);
        vm.warp(block.timestamp + 12);
        (bool refunded,) = land(address(locks), abi.encodeCall(ZecSwapLocks.refund, (id, t, e)));
        assertTrue(refunded, "the maker takes its USDC back");
        assertTrue(publicShare[z] && publicShare[e], "and holds both halves: it sweeps the user's ZEC");
    }

    /// A client built for locks pointed at the windows contract: `lockClaim` is not a function
    /// there, so the call reverts and the client never reaches `claim`. Safe, but only because the
    /// lock flow puts the lock first: its "claims never expire" rule would leak after t1.
    function test_lockClientOnTheWindowsContractFailsBeforeRevealing() public {
        Case memory c =
            openForward(windows, maker, 51, 52, uint64(START + 90 minutes), uint64(START + 150 minutes));
        vm.warp(c.terms.t0 + 1);
        ZecSwapLocks.Terms memory t = ZecSwapLocks.Terms(
            c.terms.maker,
            c.terms.token,
            c.terms.amount,
            c.terms.makerKey,
            c.terms.userKey,
            c.terms.user,
            c.terms.t0,
            c.terms.t1,
            c.terms.payoutNote
        );
        vm.prank(c.terms.user);
        (bool locked,) = address(windows).call(abi.encodeCall(ZecSwapLocks.lockClaim, (c.id, t)));
        assertFalse(locked, "no such function");
        assertFalse(publicShare[c.claimShare]);
        vm.warp(c.terms.t1 + 1);
        (bool claimed, bytes4 sel) =
            land(address(windows), abi.encodeCall(ZecSwap.claim, (c.id, c.terms, c.claimShare)));
        assertFalse(claimed);
        assertEq(sel, ZecSwap.NotNow.selector);
        assertTrue(publicShare[c.claimShare], "a late claim under the old 'never expires' rule leaks z");
        (bool refunded,) =
            land(address(windows), abi.encodeCall(ZecSwap.refund, (c.id, c.terms, c.refundShare)));
        assertTrue(refunded, "the maker refunds in its late window and holds both");
        assertTrue(bothPublic(c));
    }

    /// The record's shape: a reader must know which contract it reads.
    function test_getSwapReturnsFourWordsOnWindowsAndSixOnLocks() public {
        Case memory c =
            openForward(windows, maker, 61, 62, uint64(START + 90 minutes), uint64(START + 150 minutes));
        (bool ok, bytes memory ret) = address(windows).staticcall(abi.encodeCall(ZecSwap.getSwap, (c.id)));
        assertTrue(ok);
        assertEq(ret.length, 4 * 32);
        (bytes32 id, ZecSwapLocks.Terms memory t,,) = lockSwap(63, 64);
        (ok, ret) = address(locks).staticcall(abi.encodeCall(ZecSwapLocks.getSwap, (id)));
        assertTrue(ok);
        assertEq(ret.length, 6 * 32);
        // Solidity's own decoder takes six words as four, reading the claim lock as the secret.
        ZecSwap.Swap memory misread = abi.decode(ret, (ZecSwap.Swap));
        assertEq(uint8(misread.stage), OPEN);
        assertEq(misread.secret, 0);
        vm.warp(t.t0 + 1);
        vm.prank(t.user);
        locks.lockClaim(id, t);
        (ok, ret) = address(locks).staticcall(abi.encodeCall(ZecSwapLocks.getSwap, (id)));
        misread = abi.decode(ret, (ZecSwap.Swap));
        assertEq(misread.secret, uint256(t.t0 + 1 + 30 minutes), "the claim lock lands in the secret field");
        // The windows contract has no LOCK_DURATION; the lock contract has no MIN_WINDOW. Either
        // call tells a client which contract it is talking to.
        (ok,) = address(windows).staticcall(abi.encodeWithSignature("LOCK_DURATION()"));
        assertFalse(ok);
        (ok,) = address(locks).staticcall(abi.encodeWithSignature("MIN_WINDOW()"));
        assertFalse(ok);
        (ok, ret) = address(windows).staticcall(abi.encodeWithSignature("MIN_WINDOW()"));
        assertTrue(ok);
        assertEq(abi.decode(ret, (uint256)), 1 hours);
        (ok, ret) = address(windows).staticcall(abi.encodeWithSignature("LATE_WINDOW()"));
        assertTrue(ok);
        assertEq(abi.decode(ret, (uint256)), 24 hours);
    }

    function lockSwap(uint256 ki, uint256 kj)
        internal
        returns (bytes32 id, ZecSwapLocks.Terms memory t, uint256 e, uint256 z)
    {
        uint256[2] memory E;
        uint256[2] memory Z;
        (e, E) = pair(ki);
        (z, Z) = pair(kj);
        address user = makeAddr(string.concat("lock-user-", vm.toString(kj)));
        t = ZecSwapLocks.Terms(
            maker,
            address(usdc),
            AMOUNT,
            E,
            Z,
            user,
            uint64(START + 90 minutes),
            uint64(START + 150 minutes),
            0
        );
        vm.prank(maker);
        id = locks.open(address(usdc), AMOUNT, E, Z, user, t.t0, t.t1, 0);
    }

    /// Lands `data` as a transaction would, and records any share it carried.
    function land(address to, bytes memory data) internal returns (bool ok, bytes4 sel) {
        bytes memory ret;
        (ok, ret) = to.call(data);
        sel = selectorOf(ret);
        if (data.length < 4 + 32 + 11 * 32 + 32) return (ok, sel);
        bytes4 f = bytes4(data);
        if (
            f == ZecSwap.claim.selector || f == ZecSwap.refund.selector || f == ZecSwapLocks.claim.selector
                || f == ZecSwapLocks.refund.selector
        ) {
            uint256 share;
            assembly {
                // id, 11 words of Terms, then the share
                share := mload(add(data, add(32, add(4, mul(12, 32)))))
            }
            publicShare[share] = true;
        }
    }

    /// Whoever knows both halves can spend the joint Orchard address.
    function bothPublic(Case memory c) internal view returns (bool) {
        return publicShare[c.claimShare] && publicShare[c.refundShare]
            && Pallas.isSpendAuthMul(c.claimShare, c.terms.userKey[0], c.terms.userKey[1])
            && Pallas.isSpendAuthMul(c.refundShare, c.terms.makerKey[0], c.terms.makerKey[1]);
    }
}
