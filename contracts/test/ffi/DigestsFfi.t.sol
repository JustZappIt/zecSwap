// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ZecSwap} from "../../src/ZecSwap.sol";
import {IRailgun} from "../../src/ShieldVault.sol";
import {WindowsBase} from "../utils/WindowsBase.sol";

/// Audit 3 (area 8): zecswap-core's `Domain` digests against the windows contract's
/// `_checkSignature`, on random inputs. The lock digests are gone; these five remain and must
/// agree to the byte, or an honest reveal's authorization fails on chain. Two are also exercised
/// live: a `readyWithSig` and an `openReverse` signed over Rust's digest land.
contract DigestsFfiTest is WindowsBase {
    ZecSwap internal swaps;

    function setUp() public override {
        vm.skip(keccak256(bytes(vm.envOr("FOUNDRY_PROFILE", string("")))) != keccak256("ffi"));
        super.setUp();
        swaps = deploy(1 hours, 24 hours);
    }

    function digestOf(bytes32 structHash) internal view returns (bytes32) {
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

    function rust(string[] memory args) internal returns (bytes32) {
        string[] memory cmd = new string[](args.length + 4);
        cmd[0] = string.concat(vm.projectRoot(), "/../target/release/examples/evm_vectors");
        cmd[1] = "digest";
        cmd[2] = args[0];
        cmd[3] = vm.toString(block.chainid);
        cmd[4] = vm.toString(address(swaps));
        for (uint256 i = 1; i < args.length; i++) {
            cmd[i + 4] = args[i];
        }
        return abi.decode(vm.ffi(cmd), (bytes32));
    }

    function testFuzz_readyDigestMatches(bytes32 id, uint64 deadline) public {
        string[] memory a = new string[](3);
        (a[0], a[1], a[2]) = ("ready", vm.toString(id), vm.toString(deadline));
        assertEq(rust(a), digestOf(keccak256(abi.encode(READY_TYPEHASH, id, deadline))));
    }

    function testFuzz_payoutDigestMatches(bytes32 id, address relayer, uint128 fee) public {
        string[] memory a = new string[](4);
        (a[0], a[1], a[2], a[3]) = ("payout", vm.toString(id), vm.toString(relayer), vm.toString(fee));
        assertEq(rust(a), digestOf(keccak256(abi.encode(PAYOUT_TYPEHASH, id, relayer, fee))));
    }

    function testFuzz_refundPayoutDigestMatches(bytes32 id, address relayer, uint128 fee) public {
        string[] memory a = new string[](4);
        (a[0], a[1], a[2], a[3]) = ("refund-payout", vm.toString(id), vm.toString(relayer), vm.toString(fee));
        assertEq(rust(a), digestOf(keccak256(abi.encode(REFUND_PAYOUT_TYPEHASH, id, relayer, fee))));
    }

    function testFuzz_rescueDigestMatches(
        bytes32 id,
        bytes32 note,
        address relayer,
        uint128 fee,
        uint64 nonce,
        uint64 deadline
    ) public {
        string[] memory a = new string[](7);
        a[0] = "rescue";
        a[1] = vm.toString(id);
        a[2] = vm.toString(note);
        a[3] = vm.toString(relayer);
        a[4] = vm.toString(fee);
        a[5] = vm.toString(nonce);
        a[6] = vm.toString(deadline);
        assertEq(
            rust(a), digestOf(keccak256(abi.encode(RESCUE_TYPEHASH, id, note, relayer, fee, nonce, deadline)))
        );
    }

    function testFuzz_openReverseDigestMatchesAndLands(
        uint256 ki,
        uint256 kj,
        uint64 t0After,
        uint64 t1After,
        uint64 deadlineAfter,
        uint96 amount
    ) public {
        // distinct points: the vectors repeat by index, and equal x coordinates are refused
        (, uint256[2] memory M) = pair(ki);
        (, uint256[2] memory U) = pair(kj);
        vm.assume(M[0] != U[0]);
        t0After = uint64(bound(t0After, 1 hours, 7 days));
        t1After = uint64(bound(t1After, t0After + 1 hours, t0After + 7 days));
        deadlineAfter = uint64(bound(deadlineAfter, 0, t0After - 1));
        Case memory c;
        ZecSwap.ReverseOpen memory r = reverseTerms(
            c,
            maker,
            ki,
            kj,
            uint64(block.timestamp) + t0After,
            uint64(block.timestamp) + t1After,
            uint64(block.timestamp) + deadlineAfter
        );
        r.amount = uint128(bound(amount, 1, type(uint96).max));
        string[] memory a = new string[](11);
        a[0] = "open-reverse";
        a[1] = vm.toString(r.maker);
        a[2] = vm.toString(r.user);
        a[3] = vm.toString(r.token);
        a[4] = vm.toString(r.amount);
        a[5] = vm.toString(abi.encodePacked(r.makerKey));
        a[6] = vm.toString(abi.encodePacked(r.userKey));
        a[7] = vm.toString(r.t0);
        a[8] = vm.toString(r.t1);
        a[9] = vm.toString(r.refundNote);
        a[10] = vm.toString(r.deadline);
        bytes32 digest = rust(a);
        bytes32 structHash = keccak256(
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
        );
        assertEq(digest, digestOf(structHash));
        // and a signature over Rust's digest opens the escrow
        (uint8 v, bytes32 rr, bytes32 ss) = vm.sign(c.authKey, digest);
        bytes memory sig = abi.encodePacked(rr, ss, v);
        usdc.mint(adapt, r.amount);
        vm.prank(adapt);
        usdc.approve(address(swaps), r.amount);
        vm.prank(adapt);
        bytes32 id = swaps.openReverse(r, sig);
        assertEq(uint8(swaps.getSwap(id).stage), OPEN);
        // and a ready signed over Rust's digest readies it
        uint64 deadline = uint64(block.timestamp + 120);
        string[] memory b = new string[](3);
        (b[0], b[1], b[2]) = ("ready", vm.toString(id), vm.toString(deadline));
        (v, rr, ss) = vm.sign(c.authKey, rust(b));
        ZecSwap.Terms memory t =
            ZecSwap.Terms(r.user, r.token, r.amount, r.userKey, r.makerKey, r.maker, r.t0, r.t1, 0);
        swaps.readyWithSig(id, t, deadline, abi.encodePacked(rr, ss, v));
        assertEq(uint8(swaps.getSwap(id).stage), READY);
    }
}
