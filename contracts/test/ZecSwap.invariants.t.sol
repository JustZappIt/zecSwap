// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {IRailgun} from "../src/ShieldVault.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {MockRailgun} from "./utils/MockRailgun.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Drives random interleavings of every action on forward swaps and reverse escrows, for two makers
/// and two tokens, with either token or Railgun paused at times, and records any departure from the
/// spec. The spec is written from the English rules alone, never from the contract's code: each
/// action's outcome is decided before it is sent, and what each account holds follows from the
/// actions that landed.
contract SwapHandler is Test {
    struct Record {
        bytes32 id;
        bool forward;
        ZecSwap.Terms terms;
        /// The share a claim reveals: the user's forward, the ZEC maker's in reverse.
        uint256 claimShare;
        /// The share a refund reveals: the maker's forward, the escrowing user's in reverse.
        uint256 refundShare;
        /// The key that signs for the swap's Railgun note and its rescues.
        uint256 signer;
        /// The note a forward claim or a reverse refund pays into Railgun; zero pays an account.
        bytes32 note;
        ZecSwap.Stage stage;
        bool paidOut;
        uint64 rescues;
    }

    enum Want {
        Readyable,
        Claimable,
        Refundable,
        Owing,
        PaidOut
    }

    /// Few enough at once that random picks keep landing on swaps in a window worth testing.
    uint256 internal constant MAX_LIVE = 4;
    uint256 internal constant MAX_SWAPS = 12;
    uint256 internal constant MIN_WINDOW = 1 hours;
    uint256 internal constant LATE_WINDOW = 24 hours;
    uint256 internal constant MAX_AMOUNT = 1e12;
    uint256 public constant ACCOUNTS = 5;
    bytes32 internal constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    bytes32 internal constant OPEN_REVERSE_TYPEHASH = keccak256(
        "OpenReverse(address maker,address user,address token,uint128 amount,bytes32 makerKey,bytes32 userKey,uint64 t0,uint64 t1,bytes32 refundNote,uint64 deadline)"
    );
    bytes32 internal constant READY_TYPEHASH = keccak256("Ready(bytes32 id,uint64 deadline)");
    bytes32 internal constant PAYOUT_TYPEHASH = keccak256("Payout(bytes32 id,address relayer,uint128 fee)");
    bytes32 internal constant REFUND_PAYOUT_TYPEHASH =
        keccak256("RefundPayout(bytes32 id,address relayer,uint128 fee)");
    bytes32 internal constant RESCUE_TYPEHASH =
        keccak256("Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)");

    ZecSwap public immutable swaps;
    MockRailgun public immutable railgun;
    address public immutable user = makeAddr("user");
    address public immutable relayer = makeAddr("relayer");
    address public immutable adapt = makeAddr("relay-adapt");
    address public immutable recipient = makeAddr("recipient");
    /// The forward Railgun user's key, which signs its payouts and rescues.
    address public immutable auth;
    /// The escrowing user's key in reverse, which signs its opens, readies, refund payouts and rescues.
    address public immutable escrower;
    address public immutable stranger;
    uint256 internal immutable authKey;
    uint256 internal immutable escrowerKey;
    uint256 internal immutable strangerKey;

    TestToken[2] public tokens;
    address[2] public makers;
    SpendAuthVectors.Vector[] internal keys;
    uint256 internal nextKey;
    Record[] internal records;
    mapping(bytes32 id => ZecSwap.Stage) internal settledAs;
    mapping(address account => uint256) internal keyOf;

    /// What each account may withdraw, by the rules.
    mapping(address account => mapping(address token => uint256)) public credited;
    /// Locked in live swaps.
    mapping(address token => uint256) public escrowed;
    /// Claimed forward Railgun swaps and refunded reverse escrows not yet paid out.
    mapping(address token => uint256) public owedToRailgun;
    mapping(address token => uint256) public minted;

    bool public settlementChanged;
    bool public stepDeparted;
    bool public inventoryDeparted;
    bool public revealInWindowFailed;
    bool public revealOutsideWindowSucceeded;
    bool public payoutMissedItsNote;
    bool public payoutRefused;

    constructor(ZecSwap swaps_, MockRailgun railgun_, SpendAuthVectors.Vector[] memory keys_) {
        swaps = swaps_;
        railgun = railgun_;
        (auth, authKey) = makeAddrAndKey("auth");
        (escrower, escrowerKey) = makeAddrAndKey("escrower");
        (stranger, strangerKey) = makeAddrAndKey("stranger");
        keyOf[escrower] = escrowerKey;
        for (uint256 i; i < keys_.length; ++i) {
            keys.push(keys_[i]);
        }
        for (uint256 k; k < 2; ++k) {
            tokens[k] = new TestToken();
            (address maker, uint256 key) = makeAddrAndKey(string.concat("maker", vm.toString(k)));
            (makers[k], keyOf[maker]) = (maker, key);
        }
        for (uint256 m; m < 2; ++m) {
            for (uint256 k; k < 2; ++k) {
                vm.prank(makers[m]);
                tokens[k].approve(address(swaps), type(uint256).max);
                if (!fundInventory(makers[m], tokens[k], 5 * MAX_AMOUNT)) inventoryDeparted = true;
            }
        }
    }

    function deposit(uint8 makerSeed, uint8 tokenSeed, uint64 amount) external {
        TestToken token = tokens[tokenSeed % 2];
        bool allowed = !token.paused();
        if (fundInventory(makers[makerSeed % 2], token, bound(amount, 1, 2 * MAX_AMOUNT)) != allowed) {
            inventoryDeparted = true;
        }
    }

    /// An account withdraws part of what it was credited, or (`tooMuch`) more than all of it.
    function withdraw(uint8 who, uint8 tokenSeed, uint256 amount, bool tooMuch) external {
        address account = accountAt(who);
        TestToken token = tokens[tokenSeed % 2];
        uint256 has = credited[account][address(token)];
        amount = tooMuch
            ? has + bound(amount, 1, MAX_AMOUNT)
            : bound(amount, 0, has < 2 * MAX_AMOUNT ? has : 2 * MAX_AMOUNT);
        bool allowed = !tooMuch && !token.paused();
        uint256 received = token.balanceOf(recipient);
        bool ok;
        vm.prank(account);
        try swaps.withdraw(address(token), amount, recipient) {
            ok = true;
            credited[account][address(token)] = less(has, amount);
        } catch {}
        if (ok != allowed || (ok && token.balanceOf(recipient) != received + amount)) {
            inventoryDeparted = true;
        }
    }

    /// A forward open, its deadlines up to a second short of what `open` must leave. An open never
    /// moves a token, so a paused one doesn't stop it.
    function openForward(
        uint8 makerSeed,
        uint8 tokenSeed,
        uint128 amount,
        uint32 t0Delay,
        uint32 window,
        bool shielded
    ) external {
        if (full()) return;
        SpendAuthVectors.Vector memory e = keys[nextKey];
        SpendAuthVectors.Vector memory z = keys[nextKey + 1];
        (uint64 t0, uint64 t1, bool timely) = deadlines(t0Delay, window);
        ZecSwap.Terms memory t = ZecSwap.Terms(
            makers[makerSeed % 2],
            address(tokens[tokenSeed % 2]),
            uint128(bound(amount, 1, MAX_AMOUNT)),
            [e.x, e.y],
            [z.x, z.y],
            shielded ? auth : user,
            t0,
            t1,
            shielded ? commitment(z.k) : bytes32(0)
        );
        bool allowed = timely && t.amount <= credited[t.maker][t.token];
        vm.prank(t.maker);
        try swaps.open(t.token, t.amount, t.makerKey, t.userKey, t.user, t.t0, t.t1, t.payoutNote) returns (
            bytes32 id
        ) {
            if (!allowed) stepDeparted = true;
            credited[t.maker][t.token] = less(credited[t.maker][t.token], t.amount);
            record(id, true, t, z.k, e.k, authKey, t.payoutNote);
        } catch {
            if (allowed) stepDeparted = true;
        }
    }

    /// A reverse escrow funded by Relay Adapt, signed by its user or (`honest` false) by someone else.
    function openReverse(
        uint8 makerSeed,
        uint8 tokenSeed,
        uint128 amount,
        uint32 t0Delay,
        uint32 window,
        bool honest
    ) external {
        if (full()) return;
        SpendAuthVectors.Vector memory m = keys[nextKey];
        SpendAuthVectors.Vector memory u = keys[nextKey + 1];
        (uint64 t0, uint64 t1, bool timely) = deadlines(t0Delay, window);
        ZecSwap.ReverseOpen memory o = ZecSwap.ReverseOpen(
            makers[makerSeed % 2],
            escrower,
            address(tokens[tokenSeed % 2]),
            uint128(bound(amount, 1, MAX_AMOUNT)),
            [m.x, m.y],
            [u.x, u.y],
            t0,
            t1,
            commitment(u.k),
            uint64(block.timestamp + 10 minutes)
        );
        bool allowed = honest && timely && !TestToken(o.token).paused();
        if (fundReverse(o, sign(honest ? escrowerKey : strangerKey, openReverseHash(o))) != allowed) {
            stepDeparted = true;
        }
    }

    /// A ready, half the time of a swap that may be made ready. `how`: 1 the other way, the maker's
    /// signature forward and the escrowing user's own call in reverse; 2 a stranger's call; 3 a
    /// signature sent after its deadline; 4 a stranger's signature; otherwise the rightful way, the
    /// maker's call forward and the escrowing user's signature in reverse.
    function ready(uint256 i, uint8 how) external {
        if (records.length == 0) return;
        Record storage r = pickHalfFor(i, Want.Readyable);
        how = fault(how, 4);
        bool allowed =
            r.stage == ZecSwap.Stage.Open && block.timestamp < r.terms.t0 && (r.forward ? how == 0 : how < 2);
        bool ok;
        if (how >= 3 || (r.forward ? how == 1 : how == 0)) {
            uint64 deadline = uint64(how == 3 ? block.timestamp - 1 : block.timestamp + 5 minutes);
            // Who readies a swap is its `terms.maker`: the maker forward, the escrowing user in reverse.
            bytes memory sig = sign(
                how == 4 ? strangerKey : keyOf[r.terms.maker],
                keccak256(abi.encode(READY_TYPEHASH, r.id, deadline))
            );
            try swaps.readyWithSig(r.id, r.terms, deadline, sig) {
                ok = true;
            } catch {}
        } else {
            vm.prank(how == 2 ? stranger : r.terms.maker);
            try swaps.ready(r.id, r.terms) {
                ok = true;
            } catch {}
        }
        if (ok != allowed) stepDeparted = true;
        if (ok) r.stage = ZecSwap.Stage.Ready;
        observe(r);
    }

    /// A claim at the current time, by anyone, with its share or (`honest` false) the other side's,
    /// half the time of a swap in its claim's window. Its success must match the window exactly,
    /// paused token or Railgun or not.
    function claim(uint256 i, bool honest) external {
        if (records.length == 0) return;
        Record storage r = pickHalfFor(i, Want.Claimable);
        bool inWindow = claimable(r);
        try swaps.claim(r.id, r.terms, honest ? r.claimShare : r.refundShare) {
            if (!inWindow || !honest) revealOutsideWindowSucceeded = true;
            settle(r, ZecSwap.Stage.Claimed);
        } catch {
            if (inWindow && honest) revealInWindowFailed = true;
        }
        observe(r);
    }

    function refund(uint256 i, bool honest) external {
        if (records.length == 0) return;
        Record storage r = pickHalfFor(i, Want.Refundable);
        bool inWindow = refundable(r);
        try swaps.refund(r.id, r.terms, honest ? r.refundShare : r.claimShare) {
            if (!inWindow || !honest) revealOutsideWindowSucceeded = true;
            settle(r, ZecSwap.Stage.Refunded);
        } catch {
            if (inWindow && honest) revealInWindowFailed = true;
        }
        observe(r);
    }

    /// The payout a relayer sends of a forward claim, or of a reverse refund, preferably of a swap
    /// that owes one. `how`: 1 to another note; 2 signed by someone else; 3 through the other
    /// direction's function; otherwise as signed, to the committed note. The fee leaves something
    /// to shield.
    function payout(uint256 i, uint8 how, uint128 fee) external {
        if (records.length == 0) return;
        Record storage r = pickFor(i, Want.Owing);
        how = fault(how, 3);
        TestToken token = TestToken(r.terms.token);
        address vault = swaps.vaultOf(r.id);
        uint256 value = token.balanceOf(vault) + r.terms.amount;
        fee = uint128(bound(fee, 0, value - 1));
        bool allowed = how == 0 && r.note != 0 && !r.paidOut
            && r.stage == (r.forward ? ZecSwap.Stage.Claimed : ZecSwap.Stage.Refunded) && !token.paused()
            && !railgun.paused();
        uint256 shields = railgun.count();
        uint256 fees = token.balanceOf(relayer);
        if (sendPayout(r, how, fee)) {
            r.paidOut = true;
            owedToRailgun[address(token)] = less(owedToRailgun[address(token)], r.terms.amount);
            if (
                !allowed || !shieldedFrom(vault, shields, noteSeed(r), value - fee)
                    || token.balanceOf(relayer) != fees + fee
            ) payoutMissedItsNote = true;
        } else if (allowed) {
            payoutRefused = true;
        }
        observe(r);
    }

    /// Railgun sends a held note back to the vault it was shielded from, preferably a paid-out
    /// swap's; anyone may send tokens to any swap's vault, too.
    function returnToVault(uint256 i, uint64 amount) external {
        if (records.length == 0) return;
        Record storage r = pickFor(i, Want.PaidOut);
        amount = uint64(bound(amount, 1, MAX_AMOUNT));
        TestToken(r.terms.token).mint(swaps.vaultOf(r.id), amount);
        minted[r.terms.token] += amount;
    }

    /// A rescue of what came back to a vault, preferably a paid-out one, to a fresh note. `how`: 1
    /// signed by someone else; 2 under another nonce than the next; 3 after its deadline; otherwise
    /// as signed. The fee leaves something to shield of whatever the vault holds.
    function rescue(uint256 i, uint8 how, uint128 fee) external {
        if (records.length == 0) return;
        Record storage r = pickFor(i, Want.PaidOut);
        how = fault(how, 3);
        TestToken token = TestToken(r.terms.token);
        address vault = swaps.vaultOf(r.id);
        uint256 value = token.balanceOf(vault);
        fee = value == 0 ? 0 : uint128(bound(fee, 0, value - 1));
        uint256 seed = uint256(keccak256(abi.encode(r.id, r.rescues)));
        bool allowed = how == 0 && r.paidOut && value > 0 && !token.paused() && !railgun.paused();
        uint256 shields = railgun.count();
        uint256 fees = token.balanceOf(relayer);
        if (sendRescue(r, how, seed, fee)) {
            ++r.rescues;
            if (
                !allowed || !shieldedFrom(vault, shields, seed, value - fee)
                    || token.balanceOf(relayer) != fees + fee
            ) payoutMissedItsNote = true;
        } else if (allowed) {
            payoutRefused = true;
        }
        observe(r);
    }

    /// Pauses (one time in eight) or resumes a token or Railgun.
    function pause(uint8 what, uint8 odds) external {
        bool on = odds % 8 == 0;
        what %= 3;
        if (what == 2) railgun.setPaused(on);
        else tokens[what].setPaused(on);
    }

    /// Mostly up to 90 minutes a step, so runs cross each window's edges; one step in eight up to
    /// 30 hours, so they reach the late window's end, a day past `t1`, too.
    function warp(uint32 by) external {
        vm.warp(block.timestamp + bound(by, 1, by % 8 == 0 ? 30 hours : 90 minutes));
    }

    /// Jumps to the second before, of or after the next of a swap's `t0`, `t1` and late window's end
    /// still ahead: where its windows change hands.
    function warpToEdge(uint256 i, uint8 offset) external {
        if (records.length == 0) return;
        ZecSwap.Terms storage t = pick(i).terms;
        uint256[3] memory edges = [uint256(t.t0), t.t1, t.t1 + LATE_WINDOW];
        for (uint256 k; k < 3; ++k) {
            uint256 at = edges[k] - 1 + offset % 3;
            if (at > block.timestamp) {
                vm.warp(at);
                return;
            }
        }
    }

    // The windows, from the rules. Forward: the user may claim a ready swap until t1, an unready
    // one from t0 until t1, and either from t1 + LATE_WINDOW for good; its maker may cancel an
    // unready swap before t0 and refund either from t1 for LATE_WINDOW. Reverse: the ZEC maker may
    // claim a ready escrow until t1; its user may refund an unready one at any time, and either one
    // from t1 for good.

    function claimable(Record storage r) internal view returns (bool) {
        if (!live(r.stage)) return false;
        if (!r.forward) return r.stage == ZecSwap.Stage.Ready && block.timestamp < r.terms.t1;
        if (block.timestamp >= r.terms.t1) return block.timestamp >= r.terms.t1 + LATE_WINDOW;
        return r.stage == ZecSwap.Stage.Ready || block.timestamp >= r.terms.t0;
    }

    function refundable(Record storage r) internal view returns (bool) {
        if (!live(r.stage)) return false;
        if (!r.forward) return r.stage == ZecSwap.Stage.Open || block.timestamp >= r.terms.t1;
        if (block.timestamp >= r.terms.t1) return block.timestamp < r.terms.t1 + LATE_WINDOW;
        return r.stage == ZecSwap.Stage.Open && block.timestamp < r.terms.t0;
    }

    // What the invariants read.

    function count() external view returns (uint256) {
        return records.length;
    }

    function recordAt(uint256 i) external view returns (Record memory) {
        return records[i];
    }

    function claimableAt(uint256 i) external view returns (bool) {
        return claimable(records[i]);
    }

    function refundableAt(uint256 i) external view returns (bool) {
        return refundable(records[i]);
    }

    /// Every account the escrow may credit: the makers, the forward account user, and the keys that
    /// only sign, which it never should.
    function accountAt(uint256 i) public view returns (address) {
        i %= ACCOUNTS;
        if (i < 2) return makers[i];
        return i == 2 ? user : i == 3 ? auth : escrower;
    }

    function vaultsHold(address token) external view returns (uint256 held) {
        for (uint256 i; i < records.length; ++i) {
            held += TestToken(token).balanceOf(swaps.vaultOf(records[i].id));
        }
    }

    // Plumbing.

    /// The maker deposits `amount` it has just been paid; whether the deposit landed.
    function fundInventory(address maker, TestToken token, uint256 amount) internal returns (bool ok) {
        token.mint(maker, amount);
        minted[address(token)] += amount;
        vm.prank(maker);
        try swaps.deposit(address(token), amount) {
            ok = true;
            credited[maker][address(token)] += amount;
        } catch {}
    }

    /// Relay Adapt's part: unshield the amount, approve exactly it and open; whether the open landed.
    function fundReverse(ZecSwap.ReverseOpen memory o, bytes memory sig) internal returns (bool ok) {
        TestToken(o.token).mint(adapt, o.amount);
        minted[o.token] += o.amount;
        vm.prank(adapt);
        TestToken(o.token).approve(address(swaps), o.amount);
        vm.prank(adapt);
        try swaps.openReverse(o, sig) returns (bytes32 id) {
            ok = true;
            ZecSwap.Terms memory t =
                ZecSwap.Terms(escrower, o.token, o.amount, o.userKey, o.makerKey, o.maker, o.t0, o.t1, 0);
            record(id, false, t, keys[nextKey].k, keys[nextKey + 1].k, escrowerKey, o.refundNote);
        } catch {}
    }

    function record(
        bytes32 id,
        bool forward,
        ZecSwap.Terms memory t,
        uint256 claimShare,
        uint256 refundShare,
        uint256 signer,
        bytes32 note
    ) internal {
        nextKey += 2;
        escrowed[t.token] += t.amount;
        Record storage r = records.push();
        r.id = id;
        r.forward = forward;
        r.terms = t;
        r.claimShare = claimShare;
        r.refundShare = refundShare;
        r.signer = signer;
        r.note = note;
        r.stage = ZecSwap.Stage.Open;
    }

    /// A claim credits the user forward (or holds a Railgun swap's amount for its payout) and the ZEC
    /// maker in reverse; a refund credits the maker forward and holds a reverse refund for Railgun.
    function settle(Record storage r, ZecSwap.Stage stage) internal {
        r.stage = stage;
        address token = r.terms.token;
        uint256 amount = r.terms.amount;
        escrowed[token] = less(escrowed[token], amount);
        bool claimed = stage == ZecSwap.Stage.Claimed;
        if (r.forward ? claimed && r.note != 0 : !claimed) owedToRailgun[token] += amount;
        else credited[claimed ? r.terms.user : r.terms.maker][token] += amount;
    }

    function sendPayout(Record storage r, uint8 how, uint128 fee) internal returns (bool ok) {
        uint256 seed = noteSeed(r);
        bytes32 npk = npkOf(how == 1 ? seed ^ 1 : seed);
        IRailgun.ShieldCiphertext memory c = cipherOf(seed);
        bool refundSide = r.forward == (how == 3);
        bytes32 typehash = refundSide ? REFUND_PAYOUT_TYPEHASH : PAYOUT_TYPEHASH;
        bytes memory sig =
            sign(how == 2 ? strangerKey : r.signer, keccak256(abi.encode(typehash, r.id, relayer, fee)));
        ZecSwap.Terms memory t = r.terms;
        vm.prank(relayer);
        if (refundSide) {
            try swaps.refundPayout(r.id, t, npk, c, fee, sig) {
                ok = true;
            } catch {}
        } else {
            try swaps.payout(r.id, t, npk, c, fee, sig) {
                ok = true;
            } catch {}
        }
    }

    function sendRescue(Record storage r, uint8 how, uint256 seed, uint128 fee) internal returns (bool ok) {
        uint64 nonce = how != 2 ? r.rescues : r.rescues == 0 ? 1 : r.rescues - 1;
        uint64 deadline = uint64(how == 3 ? block.timestamp - 1 : block.timestamp + 5 minutes);
        bytes32 npk = npkOf(seed);
        IRailgun.ShieldCiphertext memory c = cipherOf(seed);
        bytes32 note = keccak256(abi.encode(npk, c));
        bytes memory sig = sign(
            how == 1 ? strangerKey : r.signer,
            keccak256(abi.encode(RESCUE_TYPEHASH, r.id, note, relayer, fee, nonce, deadline))
        );
        ZecSwap.Terms memory t = r.terms;
        vm.prank(relayer);
        try swaps.rescue(r.id, t, npk, c, fee, nonce, deadline, sig) {
            ok = true;
        } catch {}
    }

    /// Whether the last shield is the only new one: from `vault`, to the note `seed` names, of
    /// `value` less Railgun's fee, leaving the vault empty.
    function shieldedFrom(address vault, uint256 shields, uint256 seed, uint256 value)
        internal
        view
        returns (bool)
    {
        if (railgun.count() != shields + 1) return false;
        MockRailgun.Shielded memory note = railgun.last();
        return note.from == vault && note.npk == npkOf(seed)
            && note.ciphertextHash == keccak256(abi.encode(cipherOf(seed)))
            && note.value == value - value * railgun.FEE_BPS() / 10_000
            && TestToken(note.token).balanceOf(vault) == 0;
    }

    /// `t0` and `t1` up to a second short of what an open must leave, and whether they leave it.
    function deadlines(uint32 t0Delay, uint32 window)
        internal
        view
        returns (uint64 t0, uint64 t1, bool timely)
    {
        uint256 delay = bound(t0Delay, MIN_WINDOW - 1, 2 hours);
        uint256 gap = bound(window, MIN_WINDOW - 1, 4 hours);
        t0 = uint64(block.timestamp + delay);
        t1 = uint64(t0 + gap);
        timely = delay >= MIN_WINDOW && gap >= MIN_WINDOW;
    }

    function observe(Record storage r) internal {
        ZecSwap.Stage stage = swaps.getSwap(r.id).stage;
        ZecSwap.Stage settled = settledAs[r.id];
        if (settled != ZecSwap.Stage.None && stage != settled) settlementChanged = true;
        if (settled == ZecSwap.Stage.None && !live(stage)) settledAs[r.id] = stage;
    }

    function pick(uint256 i) internal view returns (Record storage) {
        return records[i % records.length];
    }

    /// The first swap from `i` on that `want` describes, or `i`'s.
    function pickFor(uint256 i, Want want) internal view returns (Record storage) {
        i %= records.length;
        for (uint256 k; k < records.length; ++k) {
            if (wanted(pick(i + k), want)) return pick(i + k);
        }
        return pick(i);
    }

    /// `pickFor` for even `i`, any swap for odd: so the steps that are refused still get tried.
    function pickHalfFor(uint256 i, Want want) internal view returns (Record storage) {
        return i % 2 == 0 ? pickFor(i / 2, want) : pick(i / 2);
    }

    function wanted(Record storage r, Want want) internal view returns (bool) {
        if (want == Want.Readyable) return r.stage == ZecSwap.Stage.Open && block.timestamp < r.terms.t0;
        if (want == Want.Claimable) return claimable(r);
        if (want == Want.Refundable) return refundable(r);
        if (want == Want.PaidOut) return r.paidOut;
        return r.note != 0 && !r.paidOut
            && r.stage == (r.forward ? ZecSwap.Stage.Claimed : ZecSwap.Stage.Refunded);
    }

    /// Whether another swap may open: few enough live, and keys left for the run.
    function full() internal view returns (bool) {
        if (records.length == MAX_SWAPS) return true;
        uint256 open;
        for (uint256 k; k < records.length; ++k) {
            if (live(records[k].stage)) ++open;
        }
        return open == MAX_LIVE;
    }

    /// One of the `faults` faults `how` names, an eighth of the time each; otherwise none.
    function fault(uint8 how, uint8 faults) internal pure returns (uint8) {
        how %= 8;
        return how > faults ? 0 : how;
    }

    function live(ZecSwap.Stage stage) internal pure returns (bool) {
        return stage == ZecSwap.Stage.Open || stage == ZecSwap.Stage.Ready;
    }

    /// `a - b`, or zero: after a departure the model follows what landed without reverting the call
    /// that flagged it.
    function less(uint256 a, uint256 b) internal pure returns (uint256) {
        return a > b ? a - b : 0;
    }

    /// The share that tells a swap's note from the others': the user's own share in either direction.
    function noteSeed(Record storage r) internal view returns (uint256) {
        return r.forward ? r.claimShare : r.refundShare;
    }

    /// A note's commitment as wallets compute it: the ABI encoding of its `npk` and ciphertext, hashed.
    function commitment(uint256 seed) internal pure returns (bytes32) {
        return keccak256(abi.encode(npkOf(seed), cipherOf(seed)));
    }

    function npkOf(uint256 seed) internal pure returns (bytes32) {
        return keccak256(abi.encode("npk", seed));
    }

    function cipherOf(uint256 seed) internal pure returns (IRailgun.ShieldCiphertext memory c) {
        c.encryptedBundle =
            [keccak256(abi.encode(seed, 0)), keccak256(abi.encode(seed, 1)), keccak256(abi.encode(seed, 2))];
        c.shieldKey = keccak256(abi.encode(seed, "shield key"));
    }

    function openReverseHash(ZecSwap.ReverseOpen memory o) internal pure returns (bytes32) {
        return keccak256(
            abi.encode(
                OPEN_REVERSE_TYPEHASH,
                o.maker,
                o.user,
                o.token,
                o.amount,
                keccak256(abi.encode(o.makerKey)),
                keccak256(abi.encode(o.userKey)),
                o.t0,
                o.t1,
                o.refundNote,
                o.deadline
            )
        );
    }

    function sign(uint256 key, bytes32 structHash) internal view returns (bytes memory) {
        bytes32 domain = keccak256(
            abi.encode(DOMAIN_TYPEHASH, keccak256("ZecSwap"), keccak256("1"), block.chainid, address(swaps))
        );
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(key, keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, s, v);
    }
}

contract ZecSwapInvariantTest is SpendAuthVectors {
    ZecSwap internal swaps;
    MockRailgun internal railgun;
    SwapHandler internal handler;

    function setUp() public override {
        super.setUp();
        railgun = new MockRailgun();
        swaps = new ZecSwap(1 hours, 24 hours, railgun);
        Vector[] memory keys = new Vector[](RANDOM_COUNT);
        for (uint256 i; i < RANDOM_COUNT; ++i) {
            keys[i] = randomVector(i);
        }
        handler = new SwapHandler(swaps, railgun, keys);
        targetContract(address(handler));
    }

    function invariant_settlementIsFinal() public view {
        assertFalse(handler.settlementChanged());
    }

    /// Every open, ready, deposit and withdrawal lands exactly when the rules allow it, and every
    /// swap is in the stage, and paid out or not, that the actions which landed put it in.
    function invariant_everyStepFollowsTheRules() public view {
        assertFalse(handler.stepDeparted(), "an open or a ready departed from the rules");
        assertFalse(handler.inventoryDeparted(), "a deposit or a withdrawal departed from the rules");
        for (uint256 i; i < handler.count(); ++i) {
            SwapHandler.Record memory r = handler.recordAt(i);
            ZecSwap.Swap memory s = swaps.getSwap(r.id);
            assertEq(uint8(s.stage), uint8(r.stage), "a swap's stage");
            assertEq(s.paidOut, r.paidOut, "a swap's payout");
        }
    }

    /// A reveal sent in its window lands whenever it is mined in that window, whatever anyone else
    /// sent before it, and with the token or Railgun paused.
    function invariant_aRevealInItsWindowNeverFails() public view {
        assertFalse(handler.revealInWindowFailed());
    }

    function invariant_noRevealSucceedsOutsideItsWindowOrWithoutItsSecret() public view {
        assertFalse(handler.revealOutsideWindowSucceeded());
    }

    /// A payout, a reverse refund's payout and a rescue each reach only the note its party signed
    /// for (the one committed at open, for the first two), with what the vault held less the
    /// signed fee, and only when the rules allow it.
    function invariant_tokensLeaveOnlyForSignedNotes() public view {
        assertFalse(handler.payoutMissedItsNote());
    }

    /// What a swap owes Railgun can always be paid out, and what comes back to its vault always
    /// rescued: nothing but the payout and the rescue themselves spend them.
    function invariant_whatIsOwedToRailgunCanAlwaysLeave() public view {
        assertFalse(handler.payoutRefused());
    }

    /// An open swap has revealed nothing; a settled one holds exactly its winner's share.
    function invariant_theSecretIsTheWinnersShare() public view {
        for (uint256 i; i < handler.count(); ++i) {
            SwapHandler.Record memory r = handler.recordAt(i);
            uint256 secret = swaps.getSwap(r.id).secret;
            if (r.stage == ZecSwap.Stage.Claimed) assertEq(secret, r.claimShare);
            else if (r.stage == ZecSwap.Stage.Refunded) assertEq(secret, r.refundShare);
            else assertEq(secret, 0);
        }
    }

    /// What replaces the lock design's turns: at every instant exactly one side of a live swap or
    /// escrow may reveal, tried with the true shares and undone, so nobody is raced or stranded.
    function invariant_exactlyOneSideMayRevealOnEveryLiveSwap() public {
        for (uint256 i; i < handler.count(); ++i) {
            SwapHandler.Record memory r = handler.recordAt(i);
            if (r.stage != ZecSwap.Stage.Open && r.stage != ZecSwap.Stage.Ready) continue;
            bool claims;
            bool refunds;
            uint256 snapshot = vm.snapshotState();
            try swaps.claim(r.id, r.terms, r.claimShare) {
                claims = true;
            } catch {}
            vm.revertToState(snapshot);
            try swaps.refund(r.id, r.terms, r.refundShare) {
                refunds = true;
            } catch {}
            vm.revertToState(snapshot);
            assertTrue(claims != refunds, "a live swap has one side that may reveal, and one only");
            assertEq(claims, handler.claimableAt(i));
            assertEq(refunds, handler.refundableAt(i));
        }
    }

    /// Every token the escrow holds is credited to an account, locked in a live swap or owed to
    /// Railgun, and every token minted is somewhere.
    function invariant_tokensAreConserved() public view {
        for (uint256 k; k < 2; ++k) {
            TestToken token = handler.tokens(k);
            address t = address(token);
            uint256 credited;
            for (uint256 a; a < handler.ACCOUNTS(); ++a) {
                credited += handler.credited(handler.accountAt(a), t);
            }
            uint256 held = token.balanceOf(address(swaps));
            assertEq(held, credited + handler.escrowed(t) + handler.owedToRailgun(t), "what the escrow owes");
            uint256 outside = token.balanceOf(address(railgun)) + token.balanceOf(railgun.treasury())
                + token.balanceOf(handler.relayer()) + token.balanceOf(handler.recipient())
                + token.balanceOf(handler.adapt()) + token.balanceOf(handler.makers(0))
                + token.balanceOf(handler.makers(1)) + handler.vaultsHold(t);
            assertEq(held + outside, handler.minted(t), "every token minted");
        }
    }

    /// Each account holds exactly what the rules credited it: inventory never moves between
    /// makers, and a Railgun swap's claim or a reverse refund is held for Railgun, never credited.
    function invariant_eachAccountHoldsWhatTheRulesCreditIt() public view {
        for (uint256 k; k < 2; ++k) {
            address t = address(handler.tokens(k));
            for (uint256 a; a < handler.ACCOUNTS(); ++a) {
                address account = handler.accountAt(a);
                assertEq(swaps.balanceOf(account, t), handler.credited(account, t));
            }
        }
    }
}
