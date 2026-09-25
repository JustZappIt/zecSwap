// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {IRailgun} from "../src/ShieldVault.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {MockRailgun} from "./utils/MockRailgun.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Drives random interleavings of every swap action, on swaps that pay an account and swaps that
/// pay into Railgun, and records any departure from the spec.
contract SwapHandler is Test {
    /// Few enough that random picks keep landing on swaps with a live lock.
    uint256 internal constant MAX_SWAPS = 4;
    bytes32 internal constant LOCK_CLAIM_TYPEHASH = keccak256("LockClaim(bytes32 id,uint64 deadline)");
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
    mapping(bytes32 => Secrets) internal secrets;
    mapping(bytes32 => ZecSwap.Stage) internal settledAs;

    bool public settlementChanged;
    bool public revealUnderLockFailed;
    bool public revealWithoutLockSucceeded;
    bool public claimableSwapRefusedLock;
    bool public payoutMissedItsNote;
    bool public payoutRefused;
    uint256 public lockedAmount;
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
        uint64 t0 = uint64(block.timestamp + bound(t0Delay, 1, 2 hours));
        uint64 t1 = uint64(t0 + bound(window, 1, 4 hours));
        bytes32 note = shielded ? swaps.noteCommitment(npkOf(z.k), cipherOf(z.k)) : bytes32(0);

        vm.prank(maker);
        bytes32 id =
            swaps.open(address(usdc), amount, [e.x, e.y], [z.x, z.y], shielded ? auth : user, t0, t1, note);
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
        bool locks;
        if (s.payoutNote == 0) {
            vm.prank(user);
            try swaps.lockClaim(id) {
                locks = true;
            } catch {}
        } else {
            uint64 deadline = uint64(block.timestamp);
            bytes memory sig = sign(keccak256(abi.encode(LOCK_CLAIM_TYPEHASH, id, deadline)));
            vm.prank(relayer);
            try swaps.lockClaimWithSig(id, deadline, sig) {
                locks = true;
            } catch {}
        }
        // Claims never expire: once claimable, only a held lock or the maker's turn refuses it.
        if (!locks && claimable && available) claimableSwapRefusedLock = true;
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
            if (s.payoutNote != 0) unpaidAmount += s.amount;
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

    /// A payout to the committed note, or with `honest` false to another, for a fee the user
    /// signed that leaves something to shield.
    function payout(uint256 i, bool honest, uint128 fee) external {
        (bytes32 id, ZecSwap.Swap memory s) = pick(i);
        if (id == 0 || s.payoutNote == 0) return;
        fee = uint128(bound(fee, 0, s.amount - 1));
        uint256 z = secrets[id].user;
        bytes32 npk = honest ? npkOf(z) : npkOf(z ^ 1);
        bool payable_ = s.stage == ZecSwap.Stage.Claimed && !s.paidOut;
        uint256 shields = railgun.count();
        bytes memory sig = sign(keccak256(abi.encode(PAYOUT_TYPEHASH, id, relayer, fee)));
        vm.prank(relayer);
        try swaps.payout(id, npk, cipherOf(z), fee, sig) {
            MockRailgun.Shielded memory note = railgun.last();
            uint120 value = uint120(s.amount - fee);
            bool toItsNote = honest && payable_ && railgun.count() == shields + 1
                && note.from == swaps.vaultOf(id) && note.npk == npkOf(z)
                && note.ciphertextHash == keccak256(abi.encode(cipherOf(z)))
                && note.value == value - value * railgun.FEE_BPS() / 10_000;
            if (!toItsNote) payoutMissedItsNote = true;
            unpaidAmount -= s.amount;
            paidOutAmount += s.amount;
        } catch {
            if (honest && payable_) payoutRefused = true;
        }
    }

    function warp(uint32 by) external {
        vm.warp(block.timestamp + bound(by, 1, 90 minutes));
    }

    function secretsOf(bytes32 id) external view returns (uint256 makerSecret, uint256 userSecret) {
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
        swaps = new ZecSwap(2 hours, railgun);
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

    function invariant_aRevealUnderAHeldLockNeverFails() public view {
        assertFalse(handler.revealUnderLockFailed());
    }

    function invariant_noRevealSucceedsWithoutItsLockAndSecret() public view {
        assertFalse(handler.revealWithoutLockSucceeded());
    }

    function invariant_claimsNeverExpire() public view {
        assertFalse(handler.claimableSwapRefusedLock());
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

    function invariant_atMostOneLockIsHeld() public view {
        for (uint256 i; i < handler.swapCount(); ++i) {
            ZecSwap.Swap memory s = swaps.getSwap(handler.ids(i));
            assertFalse(block.timestamp < s.claimLockUntil && block.timestamp < s.refundLockUntil);
        }
    }

    function invariant_tokensAreConserved() public view {
        uint256 inventory = swaps.balanceOf(handler.maker(), address(usdc));
        uint256 claimed = swaps.balanceOf(handler.user(), address(usdc));
        uint256 held = usdc.balanceOf(address(swaps));
        assertEq(held, inventory + claimed + handler.lockedAmount() + handler.unpaidAmount());
        assertEq(held + handler.paidOutAmount(), type(uint128).max);
    }
}
