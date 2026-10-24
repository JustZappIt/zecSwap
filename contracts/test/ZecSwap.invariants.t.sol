// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {IRailgun} from "../src/ShieldVault.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {MockRailgun} from "./utils/MockRailgun.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Drives random interleavings of every swap action, on swaps that pay an account and swaps that
/// pay into Railgun, and records any departure from the spec. The spec's windows are written from
/// the English rules alone, never from the contract's code.
contract SwapHandler is Test {
    /// Few enough that random picks keep landing on swaps in a window worth testing.
    uint256 internal constant MAX_SWAPS = 4;
    uint256 internal constant MIN_WINDOW = 1 hours;
    uint256 internal constant LATE_WINDOW = 24 hours;
    bytes32 internal constant PAYOUT_TYPEHASH = keccak256("Payout(bytes32 id,address relayer,uint128 fee)");

    struct Secrets {
        uint256 maker;
        uint256 user;
    }

    ZecSwap public immutable swaps;
    TestToken public immutable usdc;
    MockRailgun public immutable railgun;
    address public immutable maker = makeAddr("maker");
    address public immutable user = makeAddr("user");
    address public immutable relayer = makeAddr("relayer");
    address public immutable auth;
    uint256 internal immutable authKey;

    SpendAuthVectors.Vector[] internal keys;
    uint256 internal nextKey;

    bytes32[] public ids;
    mapping(bytes32 => ZecSwap.Terms) internal termsOf;
    mapping(bytes32 => Secrets) internal secrets;
    mapping(bytes32 => ZecSwap.Stage) internal settledAs;

    bool public settlementChanged;
    bool public revealInWindowFailed;
    bool public revealOutsideWindowSucceeded;
    bool public payoutMissedItsNote;
    bool public payoutRefused;
    uint256 public escrowedAmount;
    uint256 public unpaidAmount;
    uint256 public paidOutAmount;

    constructor(
        ZecSwap swaps_,
        TestToken usdc_,
        MockRailgun railgun_,
        SpendAuthVectors.Vector[] memory keys_
    ) {
        swaps = swaps_;
        usdc = usdc_;
        railgun = railgun_;
        (auth, authKey) = makeAddrAndKey("auth");
        for (uint256 i; i < keys_.length; ++i) {
            keys.push(keys_[i]);
        }
        usdc.mint(maker, type(uint128).max);
        vm.startPrank(maker);
        usdc.approve(address(swaps), type(uint256).max);
        swaps.deposit(address(usdc), type(uint128).max);
        vm.stopPrank();
    }

    function open(uint128 amount, uint32 t0Delay, uint32 window, bool shielded) external {
        if (ids.length == MAX_SWAPS) return;
        SpendAuthVectors.Vector memory e = keys[nextKey++];
        SpendAuthVectors.Vector memory z = keys[nextKey++];
        amount = uint128(bound(amount, 1, 1e12));
        uint64 t0 = uint64(block.timestamp + bound(t0Delay, MIN_WINDOW, 2 hours));
        uint64 t1 = uint64(t0 + bound(window, MIN_WINDOW, 4 hours));
        bytes32 note = shielded ? swaps.noteCommitment(npkOf(z.k), cipherOf(z.k)) : bytes32(0);
        ZecSwap.Terms memory t = ZecSwap.Terms(
            maker, address(usdc), amount, [e.x, e.y], [z.x, z.y], shielded ? auth : user, t0, t1, note
        );

        vm.prank(maker);
        bytes32 id = swaps.open(t.token, t.amount, t.makerKey, t.userKey, t.user, t.t0, t.t1, t.payoutNote);
        ids.push(id);
        termsOf[id] = t;
        secrets[id] = Secrets(e.k, z.k);
        escrowedAmount += amount;
    }

    function ready(uint256 i) external {
        (bytes32 id,, ZecSwap.Terms memory t) = pick(i);
        if (id == 0) return;
        vm.prank(maker);
        try swaps.ready(id, t) {} catch {}
        observe(id);
    }

    /// A claim at the current time, by anyone, with the user's share or (`honest` false) the
    /// maker's. Its success must match the claim's window exactly.
    function claim(uint256 i, bool honest) external {
        (bytes32 id, ZecSwap.Swap memory s, ZecSwap.Terms memory t) = pick(i);
        if (id == 0) return;
        bool inWindow = claimable(s, t);
        uint256 secret = honest ? secrets[id].user : secrets[id].maker;
        try swaps.claim(id, t, secret) {
            if (!inWindow || !honest) revealOutsideWindowSucceeded = true;
            escrowedAmount -= t.amount;
            if (t.payoutNote != 0) unpaidAmount += t.amount;
        } catch {
            if (inWindow && honest) revealInWindowFailed = true;
        }
        observe(id);
    }

    function refund(uint256 i, bool honest) external {
        (bytes32 id, ZecSwap.Swap memory s, ZecSwap.Terms memory t) = pick(i);
        if (id == 0) return;
        bool inWindow = refundable(s, t);
        uint256 secret = honest ? secrets[id].maker : secrets[id].user;
        try swaps.refund(id, t, secret) {
            if (!inWindow || !honest) revealOutsideWindowSucceeded = true;
            escrowedAmount -= t.amount;
        } catch {
            if (inWindow && honest) revealInWindowFailed = true;
        }
        observe(id);
    }

    /// A payout to the committed note, or with `honest` false to another, for a fee the user
    /// signed that leaves something to shield.
    function payout(uint256 i, bool honest, uint128 fee) external {
        (bytes32 id, ZecSwap.Swap memory s, ZecSwap.Terms memory t) = pick(i);
        if (id == 0 || t.payoutNote == 0) return;
        fee = uint128(bound(fee, 0, t.amount - 1));
        uint256 z = secrets[id].user;
        bytes32 npk = honest ? npkOf(z) : npkOf(z ^ 1);
        bool payable_ = s.stage == ZecSwap.Stage.Claimed && !s.paidOut;
        uint256 shields = railgun.count();
        bytes memory sig = sign(keccak256(abi.encode(PAYOUT_TYPEHASH, id, relayer, fee)));
        vm.prank(relayer);
        try swaps.payout(id, t, npk, cipherOf(z), fee, sig) {
            MockRailgun.Shielded memory note = railgun.last();
            uint120 value = uint120(t.amount - fee);
            bool toItsNote = honest && payable_ && railgun.count() == shields + 1
                && note.from == swaps.vaultOf(id) && note.npk == npkOf(z)
                && note.ciphertextHash == keccak256(abi.encode(cipherOf(z)))
                && note.value == value - value * railgun.FEE_BPS() / 10_000;
            if (!toItsNote) payoutMissedItsNote = true;
            unpaidAmount -= t.amount;
            paidOutAmount += t.amount;
        } catch {
            if (honest && payable_) payoutRefused = true;
        }
    }

    /// Up to 90 minutes a step: the late window, a day out, takes a run many steps to reach.
    function warp(uint32 by) external {
        vm.warp(block.timestamp + bound(by, 1, 90 minutes));
    }

    // The windows, from the rules: a forward swap's user may claim a ready swap until t1, an
    // unready one from t0 until t1, and either from t1 + LATE_WINDOW for good; its maker may
    // cancel an unready swap before t0 and refund either from t1 for LATE_WINDOW.

    function claimable(ZecSwap.Swap memory s, ZecSwap.Terms memory t) public view returns (bool) {
        if (!unsettled(s)) return false;
        if (block.timestamp >= t.t1) return block.timestamp >= t.t1 + LATE_WINDOW;
        return s.stage == ZecSwap.Stage.Ready || block.timestamp >= t.t0;
    }

    function refundable(ZecSwap.Swap memory s, ZecSwap.Terms memory t) public view returns (bool) {
        if (!unsettled(s)) return false;
        if (block.timestamp >= t.t1) return block.timestamp < t.t1 + LATE_WINDOW;
        return s.stage == ZecSwap.Stage.Open && block.timestamp < t.t0;
    }

    function secretsOf(bytes32 id) external view returns (uint256 makerSecret, uint256 userSecret) {
        return (secrets[id].maker, secrets[id].user);
    }

    function termsFor(bytes32 id) external view returns (ZecSwap.Terms memory) {
        return termsOf[id];
    }

    function swapCount() external view returns (uint256) {
        return ids.length;
    }

    function pick(uint256 i)
        internal
        view
        returns (bytes32 id, ZecSwap.Swap memory s, ZecSwap.Terms memory t)
    {
        if (ids.length == 0) return (0, s, t);
        id = ids[i % ids.length];
        s = swaps.getSwap(id);
        t = termsOf[id];
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

    /// Each Railgun swap's note, told apart by its user share.
    function npkOf(uint256 z) internal pure returns (bytes32) {
        return keccak256(abi.encode("npk", z));
    }

    function cipherOf(uint256 z) internal pure returns (IRailgun.ShieldCiphertext memory c) {
        c.encryptedBundle =
            [keccak256(abi.encode(z, 0)), keccak256(abi.encode(z, 1)), keccak256(abi.encode(z, 2))];
        c.shieldKey = keccak256(abi.encode(z, "shield key"));
    }

    function sign(bytes32 structHash) internal view returns (bytes memory) {
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
            vm.sign(authKey, keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, s, v);
    }
}

contract ZecSwapInvariantTest is SpendAuthVectors {
    ZecSwap internal swaps;
    TestToken internal usdc;
    MockRailgun internal railgun;
    SwapHandler internal handler;

    function setUp() public override {
        super.setUp();
        railgun = new MockRailgun();
        swaps = new ZecSwap(1 hours, 24 hours, railgun);
        usdc = new TestToken();
        Vector[] memory keys = new Vector[](RANDOM_COUNT);
        for (uint256 i; i < RANDOM_COUNT; ++i) {
            keys[i] = randomVector(i);
        }
        handler = new SwapHandler(swaps, usdc, railgun, keys);
        targetContract(address(handler));
    }

    function invariant_settlementIsFinal() public view {
        assertFalse(handler.settlementChanged());
    }

    /// A reveal sent in its window lands whenever it is mined in that window, whatever anyone
    /// else sent before it.
    function invariant_aRevealInItsWindowNeverFails() public view {
        assertFalse(handler.revealInWindowFailed());
    }

    function invariant_noRevealSucceedsOutsideItsWindowOrWithoutItsSecret() public view {
        assertFalse(handler.revealOutsideWindowSucceeded());
    }

    /// A payout reaches only the note committed at open, for the amount less the signed fee.
    function invariant_payoutsReachOnlyTheCommittedNote() public view {
        assertFalse(handler.payoutMissedItsNote());
    }

    /// Once claimed, a Railgun swap can always be paid out: nothing but the payout itself spends it.
    function invariant_aClaimedPayoutCanAlwaysLeave() public view {
        assertFalse(handler.payoutRefused());
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

    /// What replaces the lock design's turns: at every instant exactly one side of a live swap may
    /// reveal, tried with the true shares and undone, so nobody is ever raced or stranded.
    function invariant_exactlyOneSideMayRevealOnEveryLiveSwap() public {
        for (uint256 i; i < handler.swapCount(); ++i) {
            bytes32 id = handler.ids(i);
            ZecSwap.Swap memory s = swaps.getSwap(id);
            if (s.stage != ZecSwap.Stage.Open && s.stage != ZecSwap.Stage.Ready) continue;
            ZecSwap.Terms memory t = handler.termsFor(id);
            (uint256 makerSecret, uint256 userSecret) = handler.secretsOf(id);
            bool claims;
            bool refunds;
            uint256 snapshot = vm.snapshotState();
            try swaps.claim(id, t, userSecret) {
                claims = true;
            } catch {}
            vm.revertToState(snapshot);
            try swaps.refund(id, t, makerSecret) {
                refunds = true;
            } catch {}
            vm.revertToState(snapshot);
            assertTrue(claims != refunds, "a live swap has one side that may reveal, and one only");
            assertEq(claims, handler.claimable(s, t));
            assertEq(refunds, handler.refundable(s, t));
        }
    }

    function invariant_tokensAreConserved() public view {
        uint256 inventory = swaps.balanceOf(handler.maker(), address(usdc));
        uint256 claimed = swaps.balanceOf(handler.user(), address(usdc));
        uint256 held = usdc.balanceOf(address(swaps));
        assertEq(held, inventory + claimed + handler.escrowedAmount() + handler.unpaidAmount());
        assertEq(held + handler.paidOutAmount(), type(uint128).max);
    }
}
