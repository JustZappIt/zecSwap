// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ZecSwap} from "../src/ZecSwap.sol";
import {ZecSwapReverseTest} from "./ZecSwap.reverse.t.sol";

/// The contract keeps only a hash of each swap's terms and takes the terms themselves from each
/// caller, so every function that acts on a swap must check them. Each one in `actions` is called
/// on swaps at every stage, with terms that differ in a single word, and must refuse them.
contract ZecSwapTermsTest is ZecSwapReverseTest {
    /// maker, token, amount, makerKey (2), userKey (2), user, t0, t1, payoutNote.
    uint256 internal constant TERMS_WORDS = 11;

    struct Action {
        string name;
        /// Who sends it and its calldata, which the right terms would let past the terms check.
        function(bytes32, ZecSwap.Terms memory) internal view returns (address, bytes memory) call;
    }

    struct Subject {
        string stage;
        bytes32 id;
        ZecSwap.Terms terms;
    }

    struct Shares {
        uint256 maker;
        uint256 user;
    }

    /// The secrets behind each subject's `makerKey` and `userKey`.
    mapping(bytes32 id => Shares) internal sharesOf;

    function test_everyActionRefusesTermsAlteredInAnyWord() public {
        Subject[] memory subjects = swapsAtEveryStage();
        Action[] memory list = actions();
        for (uint256 s; s < subjects.length; ++s) {
            Subject memory subject = subjects[s];
            assertEq(abi.encode(subject.terms).length, TERMS_WORDS * 32);
            for (uint256 a; a < list.length; ++a) {
                for (uint256 word; word < TERMS_WORDS; ++word) {
                    expectWrongTerms(
                        list[a],
                        subject.id,
                        altered(subject.terms, word),
                        string.concat(subject.stage, ", word ", vm.toString(word))
                    );
                }
                expectPastTheTerms(list[a], subject.id, subject.terms, subject.stage);
            }
        }
    }

    function test_termsAreCheckedAgainstTheSwapTheCallNames() public {
        (bytes32 first, ZecSwap.Terms memory firstTerms) = openForward(1);
        (bytes32 second, ZecSwap.Terms memory secondTerms) = openForward(2);
        // Consistent terms, under the very id they would open, for a swap nobody opened.
        ZecSwap.Terms memory unopened = termsWith(randomVector(6), randomVector(7));
        bytes32 unopenedId = swaps.swapId(maker, unopened.userKey);
        Action[] memory list = actions();
        for (uint256 a; a < list.length; ++a) {
            expectWrongTerms(list[a], second, firstTerms, "another swap's terms");
            expectWrongTerms(list[a], first, secondTerms, "another swap's terms");
            expectWrongTerms(list[a], unopenedId, unopened, "a swap never opened");
            expectWrongTerms(list[a], bytes32(0), firstTerms, "the zero id");
        }
    }

    /// A function added later that changes a swap must join `actions`, to be tried with wrong
    /// terms above; one that leaves swaps alone joins `untouched` here.
    function test_actionsCoverEveryFunctionThatChangesState() public view {
        string memory artifact = vm.readFile(string.concat(vm.projectRoot(), "/out/ZecSwap.sol/ZecSwap.json"));
        string[] memory changing = abi.decode(
            vm.parseJson(
                artifact,
                "$.abi[?(@.type == 'function' && @.stateMutability != 'view' && @.stateMutability != 'pure')].name"
            ),
            (string[])
        );
        // Inventory moves, and the two calls that create a swap and its terms' hash.
        string[4] memory untouched = ["deposit", "withdraw", "open", "openReverse"];
        Action[] memory list = actions();
        for (uint256 f; f < changing.length; ++f) {
            bool listed;
            for (uint256 a; a < list.length; ++a) {
                listed = listed || eq(changing[f], list[a].name);
            }
            for (uint256 u; u < untouched.length; ++u) {
                listed = listed || eq(changing[f], untouched[u]);
            }
            assertTrue(listed, string.concat(changing[f], " is in neither list"));
        }
        assertEq(changing.length, list.length + untouched.length, "a listed function is gone");
    }

    /// The known answers `cargo run -p zecswap-client --example vectors` prints for wallets'
    /// ports and zecswap-core's tests check: the same terms hash the same here.
    function test_hashTermsMatchesTheWalletVectors() public view {
        address vectorMaker = 0x09eD1F966745Be18C711C346242c0974DAd7c3e5;
        address token = 0x3333333333333333333333333333333333333333;
        address swapKey = 0x757De38c2d9880E44AB59827D1622403fBF88Ff5;
        uint256[2] memory makerKey = [
            0x187d300ebb59a5c9e7c9e61debd0b535a8a5cbbad4a5c46ac2a0597a06c13ee4,
            0x329a369cf900745cdca87863f3f00de64d16e088bb1f5da341a2977547f9080a
        ];
        uint256[2] memory userKey = [
            0x0b42629d5b3f787aba7ccde87574c13f6db6b8fccb7c5cfeb3f4e4081c756461,
            0x311662156525fcaa692aeef5d2362c2a9ab2083cd6d968c618aec72617aa925a
        ];
        ZecSwap.Terms memory paysAccount = ZecSwap.Terms(
            vectorMaker,
            token,
            150_000_000,
            makerKey,
            userKey,
            0x4444444444444444444444444444444444444444,
            1_790_003_600,
            1_790_007_200,
            bytes32(0)
        );
        assertEq(
            swaps.hashTerms(paysAccount), 0x909f60119225ae64356011c63945694ed0dc6aaaa8cd6002a897df9ab76efc66
        );
        paysAccount.user = swapKey;
        paysAccount.payoutNote = 0x5af6901ba7cb01f49785a29c4a2e57e31af3e53382ce3dd2e35678897515ffc1;
        assertEq(
            swaps.hashTerms(paysAccount), 0xb7ca9333e9e443218d19c3b8aa345fa67a671ac24060877de1498efb01c99a29
        );
        ZecSwap.Terms memory reverse = ZecSwap.Terms(
            swapKey,
            token,
            50_000_000,
            userKey,
            makerKey,
            vectorMaker,
            1_790_003_600,
            1_790_007_200,
            bytes32(0)
        );
        assertEq(swaps.hashTerms(reverse), 0xa295e46f6db15997af4a950b5e6deed6ae5d3161b6571b2f445a34bc6570cead);
    }

    function actions() internal pure returns (Action[] memory list) {
        list = new Action[](11);
        list[0] = Action("ready", readyCall);
        list[1] = Action("readyWithSig", readyWithSigCall);
        list[2] = Action("lockClaim", lockClaimCall);
        list[3] = Action("lockClaimWithSig", lockClaimWithSigCall);
        list[4] = Action("claim", claimCall);
        list[5] = Action("payout", payoutCall);
        list[6] = Action("rescue", rescueCall);
        list[7] = Action("lockRefund", lockRefundCall);
        list[8] = Action("lockRefundWithSig", lockRefundWithSigCall);
        list[9] = Action("refund", refundCall);
        list[10] = Action("refundPayout", refundPayoutCall);
    }

    function readyCall(bytes32 id, ZecSwap.Terms memory t) internal pure returns (address, bytes memory) {
        return (t.maker, abi.encodeCall(ZecSwap.ready, (id, t)));
    }

    function readyWithSigCall(bytes32 id, ZecSwap.Terms memory t)
        internal
        view
        returns (address, bytes memory)
    {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig =
            sign(keccak256(abi.encode(keccak256("Ready(bytes32 id,uint64 deadline)"), id, deadline)));
        return (relayer, abi.encodeCall(ZecSwap.readyWithSig, (id, t, deadline, sig)));
    }

    function lockClaimCall(bytes32 id, ZecSwap.Terms memory t)
        internal
        pure
        returns (address, bytes memory)
    {
        return (t.user, abi.encodeCall(ZecSwap.lockClaim, (id, t)));
    }

    function lockClaimWithSigCall(bytes32 id, ZecSwap.Terms memory t)
        internal
        view
        returns (address, bytes memory)
    {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        return
            (relayer, abi.encodeCall(ZecSwap.lockClaimWithSig, (id, t, deadline, lockClaimSig(id, deadline))));
    }

    function claimCall(bytes32 id, ZecSwap.Terms memory t) internal view returns (address, bytes memory) {
        return (relayer, abi.encodeCall(ZecSwap.claim, (id, t, sharesOf[id].user)));
    }

    function payoutCall(bytes32 id, ZecSwap.Terms memory t) internal view returns (address, bytes memory) {
        bytes memory sig = payoutSig(id, relayer, FEE);
        return (relayer, abi.encodeCall(ZecSwap.payout, (id, t, npk, ciphertext, FEE, sig)));
    }

    function rescueCall(bytes32 id, ZecSwap.Terms memory t) internal view returns (address, bytes memory) {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig = rescueSig(id, npk, ciphertext, relayer, FEE);
        return (relayer, abi.encodeCall(ZecSwap.rescue, (id, t, npk, ciphertext, FEE, 0, deadline, sig)));
    }

    function lockRefundCall(bytes32 id, ZecSwap.Terms memory t)
        internal
        pure
        returns (address, bytes memory)
    {
        return (t.maker, abi.encodeCall(ZecSwap.lockRefund, (id, t)));
    }

    function lockRefundWithSigCall(bytes32 id, ZecSwap.Terms memory t)
        internal
        view
        returns (address, bytes memory)
    {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig =
            sign(keccak256(abi.encode(keccak256("LockRefund(bytes32 id,uint64 deadline)"), id, deadline)));
        return (relayer, abi.encodeCall(ZecSwap.lockRefundWithSig, (id, t, deadline, sig)));
    }

    function refundCall(bytes32 id, ZecSwap.Terms memory t) internal view returns (address, bytes memory) {
        return (relayer, abi.encodeCall(ZecSwap.refund, (id, t, sharesOf[id].maker)));
    }

    function refundPayoutCall(bytes32 id, ZecSwap.Terms memory t)
        internal
        view
        returns (address, bytes memory)
    {
        return (
            relayer, abi.encodeCall(ZecSwap.refundPayout, (id, t, npk, ciphertext, FEE, refundPayoutSig(id)))
        );
    }

    /// Railgun swaps at each stage a call can find one in, and a reverse swap refunded into Railgun.
    function swapsAtEveryStage() internal returns (Subject[] memory list) {
        string[7] memory stages =
            ["open", "ready", "claim locked", "claimed", "paid out", "refund locked", "refunded"];
        list = new Subject[](stages.length + 1);
        for (uint256 k; k < stages.length; ++k) {
            (bytes32 id, ZecSwap.Terms memory t) = openForward(k + 1);
            list[k] = Subject(stages[k], id, t);
            if (k >= 5) {
                vm.prank(maker);
                swaps.lockRefund(id, t);
                if (k == 6) swaps.refund(id, t, sharesOf[id].maker);
                continue;
            }
            if (k >= 1) {
                vm.prank(maker);
                swaps.ready(id, t);
            }
            if (k >= 2) {
                uint64 deadline = uint64(block.timestamp + 5 minutes);
                swaps.lockClaimWithSig(id, t, deadline, lockClaimSig(id, deadline));
            }
            if (k >= 3) swaps.claim(id, t, sharesOf[id].user);
            if (k >= 4) {
                bytes memory sig = payoutSig(id, relayer, FEE);
                vm.prank(relayer);
                swaps.payout(id, t, npk, ciphertext, FEE, sig);
            }
        }
        bytes32 reverse = funded();
        sharesOf[reverse] = Shares(z.k, e.k);
        refundReverse(reverse);
        bytes memory refundSig = refundPayoutSig(reverse);
        vm.prank(relayer);
        swaps.refundPayout(reverse, reverseTerms(), npk, ciphertext, FEE, refundSig);
        list[stages.length] = Subject("reverse, refund paid out", reverse, reverseTerms());
    }

    /// A Railgun swap between shares `2k` and `2k + 1`.
    function openForward(uint256 k) internal returns (bytes32 id, ZecSwap.Terms memory t) {
        Vector memory makerShare = randomVector(2 * k);
        Vector memory userShare = randomVector(2 * k + 1);
        id = openWith(makerShare, userShare);
        t = termsWith(makerShare, userShare);
        sharesOf[id] = Shares(makerShare.k, userShare.k);
    }

    /// `terms` with one word of their ABI encoding one higher.
    function altered(ZecSwap.Terms memory terms_, uint256 word)
        internal
        pure
        returns (ZecSwap.Terms memory)
    {
        bytes memory encoded = abi.encode(terms_);
        assembly ("memory-safe") {
            let at := add(add(encoded, 0x20), mul(word, 0x20))
            mstore(at, add(mload(at), 1))
        }
        return abi.decode(encoded, (ZecSwap.Terms));
    }

    function expectWrongTerms(Action memory action, bytes32 id, ZecSwap.Terms memory t, string memory why)
        internal
    {
        (address from, bytes memory data) = action.call(id, t);
        vm.prank(from);
        (bool ok, bytes memory reason) = address(swaps).call(data);
        string memory what = string.concat(action.name, ": ", why);
        assertFalse(ok, what);
        assertEq(reason, abi.encodeWithSelector(ZecSwap.WrongTerms.selector), what);
    }

    /// The swap's own terms get the call past the terms check, whatever it meets there after.
    function expectPastTheTerms(Action memory action, bytes32 id, ZecSwap.Terms memory t, string memory stage)
        internal
    {
        uint256 snapshot = vm.snapshotState();
        (address from, bytes memory data) = action.call(id, t);
        vm.prank(from);
        (, bytes memory reason) = address(swaps).call(data);
        assertTrue(bytes4(reason) != ZecSwap.WrongTerms.selector, string.concat(action.name, " at ", stage));
        vm.revertToState(snapshot);
    }

    function eq(string memory a, string memory b) internal pure returns (bool) {
        return keccak256(bytes(a)) == keccak256(bytes(b));
    }
}
