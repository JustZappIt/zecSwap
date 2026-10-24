// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {ZecSwap} from "../../src/ZecSwap.sol";
import {IRailgun} from "../../src/ShieldVault.sol";
import {MockRailgun} from "./MockRailgun.sol";
import {SpendAuthVectors} from "./SpendAuthVectors.sol";
import {TestToken} from "./TestToken.sol";

/// The plumbing the windows audits' tests share: deployments, keys, opens in both directions,
/// signatures, and calls tried and undone so one state can answer several questions.
abstract contract WindowsBase is SpendAuthVectors {
    uint8 internal constant NONE = 0;
    uint8 internal constant OPEN = 1;
    uint8 internal constant READY = 2;
    uint8 internal constant CLAIMED = 3;
    uint8 internal constant REFUNDED = 4;

    bytes32 internal constant OPEN_REVERSE_TYPEHASH = keccak256(
        "OpenReverse(address maker,address user,address token,uint128 amount,bytes32 makerKey,bytes32 userKey,uint64 t0,uint64 t1,bytes32 refundNote,uint64 deadline)"
    );
    bytes32 internal constant READY_TYPEHASH = keccak256("Ready(bytes32 id,uint64 deadline)");
    bytes32 internal constant PAYOUT_TYPEHASH = keccak256("Payout(bytes32 id,address relayer,uint128 fee)");
    bytes32 internal constant REFUND_PAYOUT_TYPEHASH =
        keccak256("RefundPayout(bytes32 id,address relayer,uint128 fee)");
    bytes32 internal constant RESCUE_TYPEHASH =
        keccak256("Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)");

    uint128 internal constant AMOUNT = 50e6;

    TestToken internal usdc;
    MockRailgun internal railgun;
    address internal maker = makeAddr("maker");
    address internal adapt = makeAddr("relay-adapt");

    struct Case {
        ZecSwap swaps;
        bool fwd;
        bytes32 id;
        ZecSwap.Terms terms;
        uint256 claimShare; // z forward, the ZEC maker's m reverse
        uint256 refundShare; // e forward, the user's u reverse
        uint256 authKey; // reverse: the escrowing user's per-swap key (stored as terms.maker)
        address zecMaker; // reverse: the ZEC maker (stored as terms.user)
        bytes32 refundNote;
    }

    function setUp() public virtual override {
        super.setUp();
        usdc = new TestToken();
        railgun = new MockRailgun();
    }

    function deploy(uint256 minWindow, uint256 lateWindow) internal returns (ZecSwap swaps) {
        swaps = new ZecSwap(minWindow, lateWindow, railgun);
        fund(swaps, maker, 1_000_000 * uint256(AMOUNT));
    }

    function fund(ZecSwap swaps, address who, uint256 amount) internal {
        usdc.mint(who, amount);
        vm.startPrank(who);
        usdc.approve(address(swaps), type(uint256).max);
        swaps.deposit(address(usdc), amount);
        vm.stopPrank();
    }

    function pair(uint256 i) internal view returns (uint256 k, uint256[2] memory p) {
        Vector memory v = randomVector(i);
        return (v.k, [v.x, v.y]);
    }

    /// A forward swap by `maker_` with keys `ki` (e) and `kj` (z), paying an account.
    function openForward(ZecSwap swaps, address maker_, uint256 ki, uint256 kj, uint64 t0, uint64 t1)
        internal
        returns (Case memory c)
    {
        uint256[2] memory E;
        uint256[2] memory Z;
        (c.refundShare, E) = pair(ki);
        (c.claimShare, Z) = pair(kj);
        address user;
        (user, c.authKey) = makeAddrAndKey(string.concat("fwd-user-", vm.toString(kj)));
        c.terms = ZecSwap.Terms(maker_, address(usdc), AMOUNT, E, Z, user, t0, t1, bytes32(0));
        c.swaps = swaps;
        c.fwd = true;
        vm.prank(maker_);
        c.id = swaps.open(address(usdc), AMOUNT, E, Z, user, t0, t1, bytes32(0));
    }

    /// A reverse escrow: `ki` is the ZEC maker's key (m), `kj` the escrowing user's (u).
    function openReverse(
        ZecSwap swaps,
        address zecMaker,
        uint256 ki,
        uint256 kj,
        uint64 t0,
        uint64 t1,
        uint64 deadline
    ) internal returns (Case memory c) {
        ZecSwap.ReverseOpen memory r = reverseTerms(c, zecMaker, ki, kj, t0, t1, deadline);
        c.id = fundReverse(swaps, r, signOpenReverse(swaps, c.authKey, r));
        c.swaps = swaps;
        c.fwd = false;
        c.zecMaker = zecMaker;
        c.terms =
            ZecSwap.Terms(r.user, address(usdc), AMOUNT, r.userKey, r.makerKey, zecMaker, t0, t1, bytes32(0));
    }

    /// The `ReverseOpen` a user signs; fills `c`'s shares, key and note.
    function reverseTerms(
        Case memory c,
        address zecMaker,
        uint256 ki,
        uint256 kj,
        uint64 t0,
        uint64 t1,
        uint64 deadline
    ) internal returns (ZecSwap.ReverseOpen memory r) {
        (uint256 m, uint256[2] memory M) = pair(ki);
        (uint256 u, uint256[2] memory U) = pair(kj);
        address auth;
        (auth, c.authKey) = makeAddrAndKey(string.concat("rev-auth-", vm.toString(ki), "-", vm.toString(kj)));
        c.refundNote = keccak256(abi.encode("refund note", ki, kj));
        c.claimShare = m;
        c.refundShare = u;
        r = ZecSwap.ReverseOpen(zecMaker, auth, address(usdc), AMOUNT, M, U, t0, t1, c.refundNote, deadline);
    }

    /// Relay Adapt's part: approve the exact amount and open.
    function fundReverse(ZecSwap swaps, ZecSwap.ReverseOpen memory r, bytes memory sig)
        internal
        returns (bytes32 id)
    {
        usdc.mint(adapt, r.amount);
        vm.prank(adapt);
        usdc.approve(address(swaps), r.amount);
        vm.prank(adapt);
        id = swaps.openReverse(r, sig);
    }

    function signOpenReverse(ZecSwap swaps, uint256 key, ZecSwap.ReverseOpen memory r)
        internal
        view
        returns (bytes memory)
    {
        return sign(
            swaps,
            key,
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
    }

    function sign(ZecSwap swaps, uint256 key, bytes32 structHash) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digestOf(swaps, structHash));
        return abi.encodePacked(r, s, v);
    }

    /// The EIP-712 digest a wallet computes for `swaps`.
    function digestOf(ZecSwap swaps, bytes32 structHash) internal view returns (bytes32) {
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
        return keccak256(abi.encodePacked("\x19\x01", domain, structHash));
    }

    function readySig(Case memory c, uint64 deadline) internal view returns (bytes memory) {
        return sign(c.swaps, c.authKey, keccak256(abi.encode(READY_TYPEHASH, c.id, deadline)));
    }

    function stageOf(Case memory c) internal view returns (uint8) {
        return uint8(c.swaps.getSwap(c.id).stage);
    }

    /// Readies `c` the way its direction does: the maker's own call forward, the user's signature
    /// (good until `sigDeadline`) sent by anyone in reverse. Not undone.
    function doReady(Case memory c, uint64 sigDeadline) internal returns (bool ok, bytes4 sel) {
        bytes memory ret;
        if (c.fwd) {
            vm.prank(c.terms.maker);
            (ok, ret) = address(c.swaps).call(abi.encodeCall(ZecSwap.ready, (c.id, c.terms)));
        } else {
            (ok, ret) = address(c.swaps).call(
                abi.encodeCall(ZecSwap.readyWithSig, (c.id, c.terms, sigDeadline, readySig(c, sigDeadline)))
            );
        }
        sel = selectorOf(ret);
    }

    function doClaim(Case memory c, uint256 share) internal returns (bool ok, bytes4 sel) {
        bytes memory ret;
        (ok, ret) = address(c.swaps).call(abi.encodeCall(ZecSwap.claim, (c.id, c.terms, share)));
        sel = selectorOf(ret);
    }

    function doRefund(Case memory c, uint256 share) internal returns (bool ok, bytes4 sel) {
        bytes memory ret;
        (ok, ret) = address(c.swaps).call(abi.encodeCall(ZecSwap.refund, (c.id, c.terms, share)));
        sel = selectorOf(ret);
    }

    /// Whether the claim (with its true share) would succeed now; the state is left as it was.
    function claims(Case memory c) internal returns (bool ok, bytes4 sel) {
        uint256 snap = vm.snapshotState();
        (ok, sel) = doClaim(c, c.claimShare);
        vm.revertToState(snap);
    }

    function refunds(Case memory c) internal returns (bool ok, bytes4 sel) {
        uint256 snap = vm.snapshotState();
        (ok, sel) = doRefund(c, c.refundShare);
        vm.revertToState(snap);
    }

    function selectorOf(bytes memory ret) internal pure returns (bytes4 sel) {
        if (ret.length >= 4) {
            assembly {
                sel := mload(add(ret, 0x20))
            }
        }
    }
}
