// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IRailgun, ShieldVault} from "../src/ShieldVault.sol";
import {Token} from "../src/Token.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {MockRailgun} from "./utils/MockRailgun.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Swaps that pay into Railgun: the user is a per-swap key that only signs, relayers send.
contract ZecSwapRailgunTest is SpendAuthVectors {
    uint256 internal constant LOCK = 2 hours;
    uint128 internal constant AMOUNT = 500e6;
    uint128 internal constant FEE = 3e6;
    uint256 internal constant INVENTORY = 10_000e6;
    uint256 internal constant SECP256K1_N = 0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141;

    ZecSwap internal swaps;
    MockRailgun internal railgun;
    TestToken internal usdc;
    address internal maker = makeAddr("maker");
    address internal relayer = makeAddr("relayer");
    address internal auth;
    uint256 internal authKey;

    Vector internal e;
    Vector internal z;
    uint64 internal t0;
    uint64 internal t1;
    bytes32 internal npk = keccak256("npk");
    IRailgun.ShieldCiphertext internal ciphertext;

    function setUp() public override {
        super.setUp();
        railgun = new MockRailgun();
        swaps = new ZecSwap(LOCK, railgun);
        usdc = new TestToken();
        usdc.mint(maker, INVENTORY);
        vm.startPrank(maker);
        usdc.approve(address(swaps), type(uint256).max);
        swaps.deposit(address(usdc), INVENTORY);
        vm.stopPrank();

        (auth, authKey) = makeAddrAndKey("auth");
        e = randomVector(0);
        z = randomVector(1);
        t0 = uint64(block.timestamp + 45 minutes);
        t1 = uint64(block.timestamp + 105 minutes);
        ciphertext = cipher("ciphertext");
    }

    // the claim lock, by signature

    function test_lockClaimWithSig_isTheSignersAndAnyoneSendsIt() public {
        bytes32 id = openReady();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        vm.prank(relayer);
        swaps.lockClaimWithSig(id, deadline, lockClaimSig(id, deadline));
        assertEq(swaps.getSwap(id).claimLockUntil, block.timestamp + LOCK);

        vm.prank(relayer);
        swaps.claim(id, z.k);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Claimed));
    }

    function test_lockClaimWithSig_takesOneLockPerSignature() public {
        bytes32 id = openReady();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig = lockClaimSig(id, deadline);
        swaps.lockClaimWithSig(id, deadline, sig);
        vm.expectRevert(ZecSwap.LockUnavailable.selector);
        swaps.lockClaimWithSig(id, deadline, sig);

        vm.warp(block.timestamp + 2 * LOCK);
        vm.expectRevert(ZecSwap.Expired.selector);
        swaps.lockClaimWithSig(id, deadline, sig);
    }

    function test_lockClaimWithSig_rejectsExpiredAndDistantDeadlines() public {
        bytes32 id = openReady();
        uint64 past = uint64(block.timestamp - 1);
        vm.expectRevert(ZecSwap.Expired.selector);
        swaps.lockClaimWithSig(id, past, lockClaimSig(id, past));

        uint64 distant = uint64(block.timestamp + LOCK);
        vm.expectRevert(ZecSwap.InvalidDeadlines.selector);
        swaps.lockClaimWithSig(id, distant, lockClaimSig(id, distant));
    }

    function test_lockClaimWithSig_keepsTheLockRules() public {
        bytes32 id = open();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.lockClaimWithSig(id, deadline, lockClaimSig(id, deadline));

        vm.prank(maker);
        swaps.lockRefund(id);
        vm.warp(t0);
        deadline = uint64(block.timestamp + 5 minutes);
        vm.expectRevert(ZecSwap.LockUnavailable.selector);
        swaps.lockClaimWithSig(id, deadline, lockClaimSig(id, deadline));
    }

    function test_signatures_authoriseOnlyTheirSwapActionChainAndContract() public {
        bytes32 id = openReady();
        uint64 deadline = uint64(block.timestamp + 5 minutes);

        bytes32 other = openWith(randomVector(2), randomVector(3));
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockClaimWithSig(other, deadline, lockClaimSig(id, deadline));

        // A payout signature over the same words is not a lock signature.
        bytes memory payoutAsLock = sign(keccak256(abi.encode(PAYOUT_TYPEHASH, id, deadline)));
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockClaimWithSig(id, deadline, payoutAsLock);

        bytes memory sig = lockClaimSig(id, deadline);
        uint256 chain = block.chainid;
        vm.chainId(chain + 1);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockClaimWithSig(id, deadline, sig);
        vm.chainId(chain);

        ZecSwap twin = new ZecSwap(LOCK, railgun);
        vm.startPrank(maker);
        usdc.approve(address(twin), AMOUNT);
        usdc.mint(maker, AMOUNT);
        twin.deposit(address(usdc), AMOUNT);
        bytes32 twinId = twin.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], auth, t0, t1, commitment());
        twin.ready(twinId);
        vm.stopPrank();
        assertEq(twinId, id);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        twin.lockClaimWithSig(id, deadline, sig);

        swaps.lockClaimWithSig(id, deadline, sig);
    }

    function test_signatures_mustBeTheUsersAndUnmalleated() public {
        bytes32 id = openReady();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes32 digest = digestOf(keccak256(abi.encode(LOCK_CLAIM_TYPEHASH, id, deadline)));

        (, uint256 strangerKey) = makeAddrAndKey("stranger");
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(strangerKey, digest);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockClaimWithSig(id, deadline, abi.encodePacked(r, s, v));

        (v, r, s) = vm.sign(authKey, digest);
        bytes memory malleated = abi.encodePacked(r, bytes32(SECP256K1_N - uint256(s)), v == 27 ? 28 : 27);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockClaimWithSig(id, deadline, malleated);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.lockClaimWithSig(id, deadline, abi.encodePacked(r, s));

        swaps.lockClaimWithSig(id, deadline, abi.encodePacked(r, s, v));
    }

    // payout

    function test_claim_holdsAShieldedPayoutForRailgun() public {
        bytes32 id = claimed();
        ZecSwap.Swap memory s = swaps.getSwap(id);
        assertEq(uint8(s.stage), uint8(ZecSwap.Stage.Claimed));
        assertEq(s.payoutNote, commitment());
        assertFalse(s.paidOut);
        assertEq(swaps.balanceOf(auth, address(usdc)), 0);
        assertEq(usdc.balanceOf(address(swaps)), INVENTORY);
    }

    function test_payout_shieldsToTheCommittedNoteLessTheSignedFee() public {
        bytes32 id = claimed();
        vm.expectEmit(address(swaps));
        emit ZecSwap.PaidOut(id, relayer, FEE);
        vm.prank(relayer);
        swaps.payout(id, npk, ciphertext, FEE, payoutSig(id, relayer, FEE));

        MockRailgun.Shielded memory note = railgun.last();
        uint120 value = uint120(AMOUNT - FEE);
        assertEq(note.from, swaps.vaultOf(id));
        assertEq(note.npk, npk);
        assertEq(note.token, address(usdc));
        assertEq(note.value, value - value * railgun.FEE_BPS() / 10_000);
        assertEq(note.ciphertextHash, keccak256(abi.encode(ciphertext)));
        assertEq(usdc.balanceOf(relayer), FEE);
        assertEq(usdc.balanceOf(address(swaps)), INVENTORY - AMOUNT);
        assertEq(usdc.balanceOf(swaps.vaultOf(id)), 0);
        assertTrue(swaps.getSwap(id).paidOut);
    }

    function test_payout_goesOnlyToTheCommittedNote() public {
        bytes32 id = claimed();
        bytes memory sig = payoutSig(id, relayer, FEE);
        vm.startPrank(relayer);
        vm.expectRevert(ZecSwap.WrongNote.selector);
        swaps.payout(id, keccak256("another npk"), ciphertext, FEE, sig);
        vm.expectRevert(ZecSwap.WrongNote.selector);
        swaps.payout(id, npk, cipher("another ciphertext"), FEE, sig);
    }

    function test_payout_isSentOnlyByTheRelayerAndForTheFeeTheUserSigned() public {
        bytes32 id = claimed();
        bytes memory sig = payoutSig(id, relayer, FEE);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        vm.prank(makeAddr("front-runner"));
        swaps.payout(id, npk, ciphertext, FEE, sig);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        vm.prank(relayer);
        swaps.payout(id, npk, ciphertext, FEE + 1, sig);
    }

    function test_payout_onlyOnceAndOnlyAfterTheClaim() public {
        bytes32 id = openReady();
        bytes memory sig = payoutSig(id, relayer, FEE);
        vm.startPrank(relayer);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.payout(id, npk, ciphertext, FEE, sig);
        vm.stopPrank();

        lockAndClaim(id);
        vm.startPrank(relayer);
        swaps.payout(id, npk, ciphertext, FEE, sig);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.payout(id, npk, ciphertext, FEE, sig);
    }

    function test_payout_failingNeverHoldsUpTheReveal() public {
        bytes32 id = openReady();
        railgun.setPaused(true);
        usdc.setPaused(true);
        lockAndClaim(id);
        assertEq(uint8(swaps.getSwap(id).stage), uint8(ZecSwap.Stage.Claimed));

        bytes memory sig = payoutSig(id, relayer, FEE);
        vm.prank(relayer);
        vm.expectRevert(Token.TransferFailed.selector);
        swaps.payout(id, npk, ciphertext, FEE, sig);
        usdc.setPaused(false);
        vm.prank(relayer);
        vm.expectRevert("paused");
        swaps.payout(id, npk, ciphertext, FEE, sig);

        railgun.setPaused(false);
        vm.prank(relayer);
        swaps.payout(id, npk, ciphertext, FEE, sig);
        assertEq(railgun.count(), 1);
    }

    function test_payout_isNotForSwapsThatPayAnAccount() public {
        vm.prank(maker);
        bytes32 id = swaps.open(address(usdc), AMOUNT, [e.x, e.y], [z.x, z.y], auth, t0, t1, bytes32(0));
        vm.prank(maker);
        swaps.ready(id);
        lockAndClaim(id);
        assertEq(swaps.balanceOf(auth, address(usdc)), AMOUNT);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.payout(id, npk, ciphertext, FEE, payoutSig(id, address(this), FEE));
    }

    function test_refund_ofARailgunSwapReturnsTheMakersInventory() public {
        bytes32 id = open();
        vm.startPrank(maker);
        swaps.lockRefund(id);
        swaps.refund(id, e.k);
        assertEq(swaps.balanceOf(maker, address(usdc)), INVENTORY);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.payout(id, npk, ciphertext, FEE, payoutSig(id, maker, FEE));
    }

    // rescue

    function test_rescue_shieldsWhatRailgunSentBackToANoteTheUserSigns() public {
        bytes32 id = paidOut();
        address vault = swaps.vaultOf(id);
        uint256 returned = railgun.last().value;
        usdc.mint(vault, returned);

        bytes32 freshNpk = keccak256("fresh npk");
        IRailgun.ShieldCiphertext memory fresh = cipher("fresh ciphertext");
        vm.expectEmit(address(swaps));
        emit ZecSwap.Rescued(id, relayer, FEE);
        vm.prank(relayer);
        swaps.rescue(id, freshNpk, fresh, FEE, 0, uint64(block.timestamp + 5 minutes), rescueSig(id, freshNpk, fresh, relayer, FEE));

        MockRailgun.Shielded memory note = railgun.last();
        assertEq(note.from, vault);
        assertEq(note.npk, freshNpk);
        assertEq(note.ciphertextHash, keccak256(abi.encode(fresh)));
        assertEq(usdc.balanceOf(vault), 0);
        assertEq(usdc.balanceOf(relayer), 2 * FEE);
    }

    function test_rescue_needsAPayoutAndTheUsersSignature() public {
        bytes32 id = claimed();
        bytes memory sig = rescueSig(id, npk, ciphertext, relayer, FEE);
        vm.prank(relayer);
        vm.expectRevert(ZecSwap.WrongStage.selector);
        swaps.rescue(id, npk, ciphertext, FEE, 0, uint64(block.timestamp + 5 minutes), sig);

        vm.prank(relayer);
        swaps.payout(id, npk, ciphertext, FEE, payoutSig(id, relayer, FEE));
        usdc.mint(swaps.vaultOf(id), AMOUNT);
        vm.prank(relayer);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        swaps.rescue(id, keccak256("the relayer's own npk"), ciphertext, FEE, 0, uint64(block.timestamp + 5 minutes), sig);
    }

    function test_vault_takesOrdersOnlyFromTheEscrow() public {
        bytes32 id = paidOut();
        ShieldVault vault = ShieldVault(swaps.vaultOf(id));
        vm.expectRevert(ShieldVault.NotEscrow.selector);
        vault.shield(railgun, address(usdc), npk, ciphertext, 0, address(this));
    }

    function testRescueApprovalIsConsumedAndCannotSpendALaterReturn() public {
        bytes32 id = paidOut();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig = rescueSig(id, npk, ciphertext, relayer, FEE);
        usdc.mint(swaps.vaultOf(id), AMOUNT);
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(swaps.rescueNonces(id), 1);
        usdc.mint(swaps.vaultOf(id), AMOUNT);
        uint256 before = usdc.balanceOf(relayer);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(usdc.balanceOf(relayer), before);
        bytes32 note = keccak256(abi.encode(npk, ciphertext));
        bytes memory fresh = sign(keccak256(abi.encode(RESCUE_TYPEHASH, id, note, relayer, FEE, uint64(1), deadline)));
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 1, deadline, fresh);
        assertEq(swaps.rescueNonces(id), 2);
    }

    function testRescueApprovalExpiresWithoutBeingConsumed() public {
        bytes32 id = paidOut();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig = rescueSig(id, npk, ciphertext, relayer, FEE);
        usdc.mint(swaps.vaultOf(id), AMOUNT);
        vm.warp(deadline + 1);
        vm.expectRevert(ZecSwap.Expired.selector);
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(swaps.rescueNonces(id), 0);
    }

    function testRescueNonceAndDeadlineAreSigned() public {
        bytes32 id = paidOut();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig = rescueSig(id, npk, ciphertext, relayer, FEE);
        usdc.mint(swaps.vaultOf(id), AMOUNT);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 0, deadline + 1, sig);
        vm.expectRevert(ZecSwap.BadSignature.selector);
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 1, deadline, sig);
    }

    function testRescueFailureDoesNotConsumeApprovalOrChargeFee() public {
        bytes32 id = paidOut();
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        bytes memory sig = rescueSig(id, npk, ciphertext, relayer, FEE);
        address vault = swaps.vaultOf(id);
        usdc.mint(vault, AMOUNT);
        uint256 feesBefore = usdc.balanceOf(relayer);
        railgun.setPaused(true);
        vm.expectRevert("paused");
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(swaps.rescueNonces(id), 0);
        assertEq(usdc.balanceOf(relayer), feesBefore);
        assertEq(usdc.balanceOf(vault), AMOUNT);
        railgun.setPaused(false);
        vm.prank(relayer);
        swaps.rescue(id, npk, ciphertext, FEE, 0, deadline, sig);
        assertEq(swaps.rescueNonces(id), 1);
    }

    // The same fixed vector as the Rust/JNI/Kotlin signer tests, decoded independently here.
    function testRescueTypedDataMatchesNativeSignerVector() public pure {
        bytes32 domain = keccak256(abi.encode(
            keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
            keccak256("ZecSwap"), keccak256("1"), uint256(11155111), address(0x1111111111111111111111111111111111111111)
        ));
        bytes32 typed = keccak256(abi.encode(
            RESCUE_TYPEHASH,
            bytes32(0xf222c5c748f566811318f3e2851848301cf248bb706b32a98278936350465ed7),
            bytes32(0x5af6901ba7cb01f49785a29c4a2e57e31af3e53382ce3dd2e35678897515ffc1),
            address(0x2222222222222222222222222222222222222222), uint128(20000), uint64(0), uint64(1790000000)
        ));
        address recovered = ecrecover(keccak256(abi.encodePacked("\x19\x01", domain, typed)), 27,
            0x91eab39afaafa2c37bfd18b4b64436386a8e2d19bb04e866f5178cc8f1878894,
            0x4999e9a2c9fabac3cda546d0fd22ad9faa53ca75759637ab6a42975f33278bea);
        assertEq(recovered, address(0x757De38c2d9880E44AB59827D1622403fBF88Ff5));
    }

    // helpers

    bytes32 internal constant LOCK_CLAIM_TYPEHASH = keccak256("LockClaim(bytes32 id,uint64 deadline)");
    bytes32 internal constant PAYOUT_TYPEHASH = keccak256("Payout(bytes32 id,address relayer,uint128 fee)");
    bytes32 internal constant RESCUE_TYPEHASH =
        keccak256("Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)");

    function open() internal returns (bytes32) {
        return openWith(e, z);
    }

    function openWith(Vector memory makerShare, Vector memory userShare) internal returns (bytes32) {
        bytes32 note = commitment();
        vm.prank(maker);
        return swaps.open(
            address(usdc),
            AMOUNT,
            [makerShare.x, makerShare.y],
            [userShare.x, userShare.y],
            auth,
            t0,
            t1,
            note
        );
    }

    function openReady() internal returns (bytes32 id) {
        id = open();
        vm.prank(maker);
        swaps.ready(id);
    }

    function lockAndClaim(bytes32 id) internal {
        uint64 deadline = uint64(block.timestamp + 5 minutes);
        vm.startPrank(relayer);
        swaps.lockClaimWithSig(id, deadline, lockClaimSig(id, deadline));
        swaps.claim(id, z.k);
        vm.stopPrank();
    }

    function claimed() internal returns (bytes32 id) {
        id = openReady();
        lockAndClaim(id);
    }

    function paidOut() internal returns (bytes32 id) {
        id = claimed();
        vm.prank(relayer);
        swaps.payout(id, npk, ciphertext, FEE, payoutSig(id, relayer, FEE));
    }

    function commitment() internal view returns (bytes32) {
        return swaps.noteCommitment(npk, ciphertext);
    }

    function cipher(string memory seed) internal pure returns (IRailgun.ShieldCiphertext memory c) {
        for (uint256 i; i < 3; ++i) {
            c.encryptedBundle[i] = keccak256(abi.encode(seed, i));
        }
        c.shieldKey = keccak256(abi.encode(seed, "shield key"));
    }

    function lockClaimSig(bytes32 id, uint64 deadline) internal view returns (bytes memory) {
        return sign(keccak256(abi.encode(LOCK_CLAIM_TYPEHASH, id, deadline)));
    }

    function payoutSig(bytes32 id, address by, uint128 fee) internal view returns (bytes memory) {
        return sign(keccak256(abi.encode(PAYOUT_TYPEHASH, id, by, fee)));
    }

    function rescueSig(
        bytes32 id,
        bytes32 noteNpk,
        IRailgun.ShieldCiphertext memory noteCiphertext,
        address by,
        uint128 fee
    ) internal view returns (bytes memory) {
        bytes32 note = keccak256(abi.encode(noteNpk, noteCiphertext));
        return sign(keccak256(abi.encode(RESCUE_TYPEHASH, id, note, by, fee, uint64(0), uint64(block.timestamp + 5 minutes))));
    }

    function sign(bytes32 structHash) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(authKey, digestOf(structHash));
        return abi.encodePacked(r, s, v);
    }

    /// The EIP-712 digest a wallet computes for `swaps`, the contract under test.
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
}
