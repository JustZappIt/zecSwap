// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {Deposits, ISanctions} from "../src/Deposits.sol";
import {IRailgun, ShieldVault} from "../src/ShieldVault.sol";
import {MockRailgun} from "./utils/MockRailgun.sol";
import {MockSanctions} from "./utils/MockSanctions.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Public tokens shielded into their owner's Railgun balance on the owner's EIP-3009 authorization,
/// sent by a relayer that keeps the fee signed for it.
contract DepositsTest is Test {
    uint256 internal constant VALUE = 25e6;
    uint128 internal constant FEE = 150_000;
    uint256 internal constant SECP256K1_N = 0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141;
    bytes32 internal constant RESCUE_TYPEHASH = keccak256(
        "Rescue(address from,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)"
    );

    /// One `shield` call's arguments.
    struct Authorization {
        address from;
        uint256 value;
        uint256 validAfter;
        uint256 validBefore;
        bytes32 npk;
        IRailgun.ShieldCiphertext ciphertext;
        uint128 fee;
        uint8 v;
        bytes32 r;
        bytes32 s;
    }

    Deposits internal deposits;
    MockRailgun internal railgun;
    TestToken internal usdc;
    address internal relayer = makeAddr("relayer");
    address internal depositor;
    uint256 internal depositorKey;
    bytes32 internal npk = keccak256("npk");
    IRailgun.ShieldCiphertext internal ciphertext;

    function setUp() public {
        usdc = new TestToken();
        railgun = new MockRailgun();
        deposits = new Deposits(address(usdc), railgun, ISanctions(address(0)));
        (depositor, depositorKey) = makeAddrAndKey("depositor");
        ciphertext = cipher("ciphertext");
        usdc.mint(depositor, VALUE);
    }

    // shield

    function test_shield_shieldsTheValueLessTheFeeToTheSignedNote() public {
        Authorization memory a = signed(authorization(), relayer);
        bytes32 note = deposits.noteCommitment(npk, ciphertext);
        address vault = deposits.vaultOf(depositor);
        assertFalse(deposits.deposited(depositor));
        vm.expectEmit(address(deposits));
        emit Deposits.DepositShielded(depositor, VALUE - FEE, FEE, relayer, note);
        send(a);
        assertTrue(deposits.deposited(depositor));

        MockRailgun.Shielded memory shielded = railgun.last();
        uint120 value = uint120(VALUE - FEE);
        assertEq(shielded.from, vault);
        assertEq(shielded.npk, npk);
        assertEq(shielded.token, address(usdc));
        assertEq(shielded.value, value - value * railgun.FEE_BPS() / 10_000);
        assertEq(shielded.ciphertextHash, keccak256(abi.encode(ciphertext)));
        assertEq(usdc.balanceOf(relayer), FEE);
        assertEq(usdc.balanceOf(depositor), 0);
        assertEq(usdc.balanceOf(address(deposits)), 0);
        assertEq(usdc.balanceOf(vault), 0);
        assertGt(vault.code.length, 0);
        assertTrue(usdc.authorizationState(depositor, deposits.depositNonce(npk, ciphertext, FEE, relayer)));
    }

    function test_shield_aLaterDepositGoesThroughTheSameVault() public {
        send(signed(authorization(), relayer));
        address vault = deposits.vaultOf(depositor);
        usdc.mint(depositor, 2 * VALUE);
        Authorization memory later = authorization();
        later.value = 2 * VALUE;
        later.npk = keccak256("later npk");
        later.ciphertext = cipher("later ciphertext");
        send(signed(later, relayer));

        assertEq(railgun.count(), 2);
        assertEq(railgun.last().from, vault);
        assertEq(railgun.last().npk, later.npk);
        assertEq(usdc.balanceOf(relayer), 2 * FEE);
        assertEq(usdc.balanceOf(vault), 0);
    }

    function test_shield_takesWhatRailgunSentBackInAgain() public {
        send(signed(authorization(), relayer));
        address vault = deposits.vaultOf(depositor);
        uint256 returned = 7e6;
        usdc.mint(vault, returned);
        usdc.mint(depositor, VALUE);
        Authorization memory later = authorization();
        later.npk = keccak256("later npk");
        later = signed(later, relayer);
        bytes32 note = deposits.noteCommitment(later.npk, later.ciphertext);
        vm.expectEmit(address(deposits));
        emit Deposits.DepositShielded(depositor, VALUE + returned - FEE, FEE, relayer, note);
        send(later);
        assertEq(usdc.balanceOf(vault), 0);
    }

    function test_shield_isTheSignedNoteFeeAndRelayerAlone() public {
        bytes memory invalid = bytes("invalid signature");
        Authorization memory a = signed(authorization(), relayer);
        a.fee = FEE + 1;
        vm.expectRevert(invalid);
        send(a);

        a = signed(authorization(), relayer);
        a.npk = keccak256("the relayer's own npk");
        vm.expectRevert(invalid);
        send(a);

        a = signed(authorization(), relayer);
        a.ciphertext = cipher("another ciphertext");
        vm.expectRevert(invalid);
        send(a);

        // The token's own digest binds the amount and the window.
        a = signed(authorization(), relayer);
        a.value = VALUE - 1;
        vm.expectRevert(invalid);
        send(a);

        a = signed(authorization(), relayer);
        a.validBefore += 1;
        vm.expectRevert(invalid);
        send(a);

        // Signed for another relayer, or sent by anyone but the one it names: another nonce.
        a = signed(authorization(), makeAddr("another relayer"));
        vm.expectRevert(invalid);
        send(a);

        a = signed(authorization(), relayer);
        vm.prank(makeAddr("copier"));
        vm.expectRevert(invalid);
        deposits.shield(
            a.from, a.value, a.validAfter, a.validBefore, a.npk, a.ciphertext, a.fee, a.v, a.r, a.s
        );

        (, uint256 strangerKey) = makeAddrAndKey("stranger");
        a = signed(authorization(), relayer);
        bytes32 nonce = deposits.depositNonce(a.npk, a.ciphertext, a.fee, relayer);
        (a.v, a.r, a.s) = vm.sign(strangerKey, receiveDigest(a, nonce));
        vm.expectRevert(invalid);
        send(a);

        a = signed(authorization(), relayer);
        (a.v, a.s) = (a.v == 27 ? 28 : 27, bytes32(SECP256K1_N - uint256(a.s)));
        vm.expectRevert(invalid);
        send(a);

        send(signed(authorization(), relayer));
        assertEq(usdc.balanceOf(relayer), FEE);
    }

    function test_shield_onlyWithinItsWindowAndOnce() public {
        Authorization memory a = authorization();
        a.validAfter = block.timestamp;
        a = signed(a, relayer);
        vm.expectRevert(bytes("authorization is not yet valid"));
        send(a);

        a = authorization();
        a.validBefore = block.timestamp;
        a = signed(a, relayer);
        vm.expectRevert(bytes("authorization is expired"));
        send(a);

        a = signed(authorization(), relayer);
        vm.warp(a.validBefore);
        vm.expectRevert(bytes("authorization is expired"));
        send(a);
        vm.warp(a.validBefore - 1);
        send(a);

        usdc.mint(depositor, VALUE);
        vm.expectRevert(bytes("authorization is used"));
        send(a);
    }

    function test_shield_refusesASanctionedDepositor() public {
        MockSanctions oracle = new MockSanctions();
        deposits = new Deposits(address(usdc), railgun, oracle);
        oracle.sanction(depositor);
        Authorization memory a = signed(authorization(), relayer);
        vm.expectRevert(Deposits.Sanctioned.selector);
        send(a);
        assertEq(usdc.balanceOf(depositor), VALUE);
    }

    function test_shield_refusesAFeeOfTheWholeValueAndMoreThanANoteHolds() public {
        Authorization memory a = authorization();
        a.fee = uint128(VALUE);
        a = signed(a, relayer);
        vm.expectRevert(Deposits.InvalidFee.selector);
        send(a);

        a = authorization();
        a.fee = uint128(VALUE) + 1;
        a = signed(a, relayer);
        vm.expectRevert(Deposits.InvalidFee.selector);
        send(a);

        a = authorization();
        a.value = uint256(type(uint120).max) + 1;
        usdc.mint(depositor, a.value);
        a = signed(a, relayer);
        vm.expectRevert(Deposits.ValueTooLarge.selector);
        send(a);
    }

    function test_shield_failingInRailgunLeavesTheAuthorizationToSendAgain() public {
        railgun.setPaused(true);
        Authorization memory a = signed(authorization(), relayer);
        vm.expectRevert(bytes("paused"));
        send(a);
        assertFalse(usdc.authorizationState(depositor, deposits.depositNonce(npk, ciphertext, FEE, relayer)));
        assertEq(usdc.balanceOf(depositor), VALUE);
        assertEq(deposits.vaultOf(depositor).code.length, 0);

        railgun.setPaused(false);
        send(a);
        assertEq(railgun.count(), 1);
    }

    function test_depositNonce_isThisContractsOnThisChain() public {
        bytes32 nonce = deposits.depositNonce(npk, ciphertext, FEE, relayer);
        Deposits twin = new Deposits(address(usdc), railgun, ISanctions(address(0)));
        assertNotEq(twin.depositNonce(npk, ciphertext, FEE, relayer), nonce);
        uint256 chain = block.chainid;
        vm.chainId(chain + 1);
        assertNotEq(deposits.depositNonce(npk, ciphertext, FEE, relayer), nonce);
        vm.chainId(chain);

        // Signed for this contract, sent through its twin: the token's digest names this one.
        Authorization memory a = signed(authorization(), relayer);
        vm.prank(relayer);
        vm.expectRevert(bytes("invalid signature"));
        twin.shield(a.from, a.value, a.validAfter, a.validBefore, a.npk, a.ciphertext, a.fee, a.v, a.r, a.s);
    }

    function test_constructor_needsAnOracleWithCodeOnMainnet() public {
        vm.expectRevert(Deposits.NoSanctionsOracle.selector);
        new Deposits(address(usdc), railgun, ISanctions(makeAddr("no code")));
        vm.expectRevert(Deposits.ZeroAddress.selector);
        new Deposits(address(0), railgun, ISanctions(address(0)));
        vm.expectRevert(Deposits.ZeroAddress.selector);
        new Deposits(address(usdc), IRailgun(address(0)), ISanctions(address(0)));

        MockSanctions oracle = new MockSanctions();
        vm.chainId(1);
        vm.expectRevert(Deposits.NoSanctionsOracle.selector);
        new Deposits(address(usdc), railgun, ISanctions(address(0)));
        assertEq(address(new Deposits(address(usdc), railgun, oracle).SANCTIONS()), address(oracle));
    }

    function test_vault_takesOrdersOnlyFromDeposits() public {
        send(signed(authorization(), relayer));
        ShieldVault vault = ShieldVault(deposits.vaultOf(depositor));
        usdc.mint(address(vault), VALUE);
        vm.expectRevert(ShieldVault.NotEscrow.selector);
        vault.shield(railgun, address(usdc), npk, ciphertext, 0, address(this));
    }

    // rescue

    function test_rescue_shieldsWhatCameBackToANoteTheDepositorSigns() public {
        send(signed(authorization(), relayer));
        address vault = deposits.vaultOf(depositor);
        uint256 returned = railgun.last().value;
        usdc.mint(vault, returned);

        bytes32 freshNpk = keccak256("fresh npk");
        IRailgun.ShieldCiphertext memory fresh = cipher("fresh ciphertext");
        uint64 deadline = uint64(block.timestamp + 10 minutes);
        bytes memory sig = rescueSig(depositorKey, freshNpk, fresh, relayer, FEE, 0, deadline);
        bytes32 note = deposits.noteCommitment(freshNpk, fresh);
        vm.expectEmit(address(deposits));
        emit Deposits.Rescued(depositor, returned - FEE, FEE, relayer, note);
        vm.prank(relayer);
        deposits.rescue(depositor, freshNpk, fresh, FEE, 0, deadline, sig);

        MockRailgun.Shielded memory shielded = railgun.last();
        assertEq(shielded.from, vault);
        assertEq(shielded.npk, freshNpk);
        assertEq(shielded.ciphertextHash, keccak256(abi.encode(fresh)));
        assertEq(usdc.balanceOf(vault), 0);
        assertEq(usdc.balanceOf(relayer), 2 * FEE);
        assertEq(deposits.rescueNonces(depositor), 1);
    }

    function test_rescue_isTheDepositorsSignatureForThisRelayerOnce() public {
        send(signed(authorization(), relayer));
        address vault = deposits.vaultOf(depositor);
        usdc.mint(vault, VALUE);
        uint64 deadline = uint64(block.timestamp + 10 minutes);
        bytes memory sig = rescueSig(depositorKey, npk, ciphertext, relayer, FEE, 0, deadline);

        vm.prank(makeAddr("copier"));
        vm.expectRevert(Deposits.BadSignature.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);
        vm.prank(relayer);
        vm.expectRevert(Deposits.BadSignature.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE + 1, 0, deadline, sig);
        vm.prank(relayer);
        vm.expectRevert(Deposits.BadSignature.selector);
        deposits.rescue(depositor, keccak256("the relayer's own npk"), ciphertext, FEE, 0, deadline, sig);
        vm.prank(relayer);
        vm.expectRevert(Deposits.BadSignature.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline + 1, sig);

        (, uint256 strangerKey) = makeAddrAndKey("stranger");
        bytes memory stranger = rescueSig(strangerKey, npk, ciphertext, relayer, FEE, 0, deadline);
        vm.prank(relayer);
        vm.expectRevert(Deposits.BadSignature.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, stranger);

        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(depositorKey, rescueDigest(npk, ciphertext, relayer, FEE, 0, deadline));
        bytes memory malleated = abi.encodePacked(r, bytes32(SECP256K1_N - uint256(s)), v == 27 ? 28 : 27);
        vm.prank(relayer);
        vm.expectRevert(Deposits.BadSignature.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, malleated);

        vm.prank(relayer);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);
        usdc.mint(vault, VALUE);
        vm.prank(relayer);
        vm.expectRevert(Deposits.BadSignature.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);

        bytes memory next = rescueSig(depositorKey, npk, ciphertext, relayer, FEE, 1, deadline);
        vm.prank(relayer);
        deposits.rescue(depositor, npk, ciphertext, FEE, 1, deadline, next);
        assertEq(deposits.rescueNonces(depositor), 2);
    }

    function test_rescue_expiresAndNeedsAVaultWithSomethingInIt() public {
        uint64 deadline = uint64(block.timestamp + 10 minutes);
        bytes memory sig = rescueSig(depositorKey, npk, ciphertext, relayer, FEE, 0, deadline);
        vm.prank(relayer);
        vm.expectRevert(Deposits.VaultNotDeployed.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);

        send(signed(authorization(), relayer));
        vm.prank(relayer);
        vm.expectRevert(Deposits.InvalidFee.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);

        usdc.mint(deposits.vaultOf(depositor), VALUE);
        vm.warp(deadline + 1);
        vm.prank(relayer);
        vm.expectRevert(Deposits.Expired.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(deposits.rescueNonces(depositor), 0);
    }

    function test_rescue_failingConsumesNothing() public {
        send(signed(authorization(), relayer));
        address vault = deposits.vaultOf(depositor);
        usdc.mint(vault, VALUE);
        uint64 deadline = uint64(block.timestamp + 10 minutes);
        bytes memory sig = rescueSig(depositorKey, npk, ciphertext, relayer, FEE, 0, deadline);
        railgun.setPaused(true);
        vm.prank(relayer);
        vm.expectRevert(bytes("paused"));
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(deposits.rescueNonces(depositor), 0);
        assertEq(usdc.balanceOf(vault), VALUE);
        assertEq(usdc.balanceOf(relayer), FEE);

        railgun.setPaused(false);
        vm.prank(relayer);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(deposits.rescueNonces(depositor), 1);
    }

    function test_rescue_refusesASanctionedDepositor() public {
        MockSanctions oracle = new MockSanctions();
        deposits = new Deposits(address(usdc), railgun, oracle);
        send(signed(authorization(), relayer));
        usdc.mint(deposits.vaultOf(depositor), VALUE);
        oracle.sanction(depositor);
        uint64 deadline = uint64(block.timestamp + 10 minutes);
        bytes memory sig = rescueSig(depositorKey, npk, ciphertext, relayer, FEE, 0, deadline);
        vm.prank(relayer);
        vm.expectRevert(Deposits.Sanctioned.selector);
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);
    }

    // Rust's answers

    /// zecswap-core's `deposit_hashes_match_the_contracts_known_answers`: the contracts at those
    /// addresses take a shield and a rescue signed over the digests Rust computed.
    function test_hashesMatchTheRustVectors() public {
        address at = address(0x1111111111111111111111111111111111111111);
        address token = address(0x8888888888888888888888888888888888888888);
        address relayer_ = address(0x7777777777777777777777777777777777777777);
        uint256 key = 1;
        address from = vm.addr(key);
        vm.chainId(11155111);
        vm.warp(1_789_000_000);
        deployCodeTo("TestToken.sol:TestToken", "", token);
        deployCodeTo("Deposits.sol:Deposits", abi.encode(token, railgun, address(0)), at);
        Deposits vectors = Deposits(at);
        TestToken(token).mint(from, 25e6);
        bytes32 notePk = repeated(0x22);
        IRailgun.ShieldCiphertext memory note =
            IRailgun.ShieldCiphertext([repeated(0x33), repeated(0x44), repeated(0x55)], repeated(0x66));

        bytes32 nonce = vectors.depositNonce(notePk, note, 150_000, relayer_);
        assertEq(nonce, 0x6917e0f85282cee681821f5eaf494c05f60a95699f31bf72e99f45365d6f8544);
        assertEq(
            TestToken(token).DOMAIN_SEPARATOR(),
            0x71fb9e856a21ce6c4c42af22617f8c5f42bc79024fa4c2acf2f845f51140175b
        );
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(key, 0x7b8010094423feaf967f85141de01415ea756128fbbcc9b5f548d4afa05404f9);
        vm.prank(relayer_);
        vectors.shield(from, 25e6, 0, 1_790_000_000, notePk, note, 150_000, v, r, s);

        bytes32 committed = vectors.noteCommitment(notePk, note);
        assertEq(committed, 0x4df47be579bd966f5104be761d3cc5704c776403467845b092d22901e91d24b0);
        TestToken(token).mint(vectors.vaultOf(from), 1e6);
        (v, r, s) = vm.sign(key, 0xa417fb0cbab5c5f12b9ec4c5d409c4148463a48675c333823dc3ce034837fc2c);
        vm.prank(relayer_);
        vectors.rescue(from, notePk, note, 150_000, 0, 1_790_000_000, abi.encodePacked(r, s, v));
        assertEq(vectors.rescueNonces(from), 1);
    }

    // helpers

    /// Shields the depositor's whole balance to `npk`, valid for an hour.
    function authorization() internal view returns (Authorization memory a) {
        a.from = depositor;
        a.value = VALUE;
        a.validBefore = block.timestamp + 1 hours;
        a.npk = npk;
        a.ciphertext = ciphertext;
        a.fee = FEE;
    }

    /// `a`, signed by the depositor for `by` to send.
    function signed(Authorization memory a, address by) internal view returns (Authorization memory) {
        bytes32 nonce = deposits.depositNonce(a.npk, a.ciphertext, a.fee, by);
        (a.v, a.r, a.s) = vm.sign(depositorKey, receiveDigest(a, nonce));
        return a;
    }

    function send(Authorization memory a) internal {
        vm.prank(relayer);
        deposits.shield(
            a.from, a.value, a.validAfter, a.validBefore, a.npk, a.ciphertext, a.fee, a.v, a.r, a.s
        );
    }

    /// The token's EIP-712 digest of `receiveWithAuthorization` to `deposits`.
    function receiveDigest(Authorization memory a, bytes32 nonce) internal view returns (bytes32) {
        bytes32 structHash = keccak256(
            abi.encode(
                usdc.RECEIVE_WITH_AUTHORIZATION_TYPEHASH(),
                a.from,
                address(deposits),
                a.value,
                a.validAfter,
                a.validBefore,
                nonce
            )
        );
        return keccak256(abi.encodePacked("\x19\x01", usdc.DOMAIN_SEPARATOR(), structHash));
    }

    function rescueSig(
        uint256 key,
        bytes32 noteNpk,
        IRailgun.ShieldCiphertext memory noteCiphertext,
        address by,
        uint128 fee,
        uint64 nonce,
        uint64 deadline
    ) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(key, rescueDigest(noteNpk, noteCiphertext, by, fee, nonce, deadline));
        return abi.encodePacked(r, s, v);
    }

    /// The depositor's rescue, as a wallet computes it for `deposits`.
    function rescueDigest(
        bytes32 noteNpk,
        IRailgun.ShieldCiphertext memory noteCiphertext,
        address by,
        uint128 fee,
        uint64 nonce,
        uint64 deadline
    ) internal view returns (bytes32) {
        bytes32 domain = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
                ),
                keccak256("ZecSwap Deposits"),
                keccak256("1"),
                block.chainid,
                address(deposits)
            )
        );
        bytes32 note = keccak256(abi.encode(noteNpk, noteCiphertext));
        bytes32 structHash = keccak256(abi.encode(RESCUE_TYPEHASH, depositor, note, by, fee, nonce, deadline));
        return keccak256(abi.encodePacked("\x19\x01", domain, structHash));
    }

    /// The word of 32 `b`s.
    function repeated(uint8 b) internal pure returns (bytes32) {
        return bytes32(uint256(b) * (type(uint256).max / 255));
    }

    function cipher(string memory seed) internal pure returns (IRailgun.ShieldCiphertext memory c) {
        for (uint256 i; i < 3; ++i) {
            c.encryptedBundle[i] = keccak256(abi.encode(seed, i));
        }
        c.shieldKey = keccak256(abi.encode(seed, "shield key"));
    }
}
