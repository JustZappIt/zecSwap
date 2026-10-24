// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {Deposits, ISanctions} from "../src/Deposits.sol";
import {IRailgun} from "../src/ShieldVault.sol";
import {MockRailgun} from "./utils/MockRailgun.sol";
import {TestToken} from "./utils/TestToken.sol";

/// Drives random deposits, returns from Railgun and rescues, honest and altered on the way, and
/// records where every token went.
contract DepositsHandler is Test {
    uint256 internal constant DEPOSITORS = 3;
    bytes32 internal constant RESCUE_TYPEHASH = keccak256(
        "Rescue(address from,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)"
    );

    Deposits public immutable deposits;
    TestToken public immutable usdc;
    MockRailgun public immutable railgun;
    address public immutable relayer = makeAddr("relayer");
    address public immutable copier = makeAddr("copier");

    address[] public depositors;
    uint256[] internal keys;
    mapping(address vault => address) public ownerOf;
    /// The notes each depositor signed for, by `noteKey`.
    mapping(address depositor => mapping(bytes32 note => bool)) public signedFor;
    /// The fees signed for the relayer, of what landed.
    uint256 public fees;
    uint256 public minted;
    bool public alteredLanded;
    bool public honestFailed;

    constructor(Deposits deposits_, TestToken usdc_, MockRailgun railgun_) {
        deposits = deposits_;
        usdc = usdc_;
        railgun = railgun_;
        for (uint256 i; i < DEPOSITORS; ++i) {
            (address depositor, uint256 key) = makeAddrAndKey(string.concat("depositor", vm.toString(i)));
            depositors.push(depositor);
            keys.push(key);
            ownerOf[deposits.vaultOf(depositor)] = depositor;
        }
    }

    /// Some token arrives at a deposit address, whose owner signs to shield its whole balance;
    /// sent as signed, or with its sender, fee or note altered.
    function deposit(uint256 who, uint96 amount, uint96 fee, uint256 seed, uint8 alteration) external {
        uint256 i = who % DEPOSITORS;
        address from = depositors[i];
        mint(from, bound(amount, 1, 1e15));
        uint256 value = usdc.balanceOf(from);
        uint128 signedFee = uint128(bound(fee, 0, value - 1));
        (bytes32 npk, IRailgun.ShieldCiphertext memory c) = note(seed);
        uint256 validBefore = block.timestamp + 1 hours;
        bytes32 nonce = deposits.depositNonce(npk, c, signedFee, relayer);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(keys[i], receiveDigest(from, value, validBefore, nonce));
        signedFor[from][noteKey(npk, c)] = true;

        alteration %= 4;
        uint128 sentFee = alteration == 1 ? signedFee + 1 : signedFee;
        bytes32 sentNpk = alteration == 2 ? keccak256(abi.encode(npk)) : npk;
        vm.prank(alteration == 3 ? copier : relayer);
        try deposits.shield(from, value, 0, validBefore, sentNpk, c, sentFee, v, r, s) {
            if (alteration != 0) alteredLanded = true;
            fees += signedFee;
        } catch {
            if (alteration == 0) honestFailed = true;
        }
    }

    /// Railgun sends a note back to the vault it came from.
    function returnToVault(uint256 who, uint96 amount) external {
        address vault = deposits.vaultOf(depositors[who % DEPOSITORS]);
        if (vault.code.length == 0) return;
        mint(vault, bound(amount, 1, 1e15));
    }

    function rescue(uint256 who, uint96 fee, uint256 seed, uint8 alteration) external {
        uint256 i = who % DEPOSITORS;
        address from = depositors[i];
        uint256 held = usdc.balanceOf(deposits.vaultOf(from));
        if (held == 0) return;
        uint128 signedFee = uint128(bound(fee, 0, held - 1));
        (bytes32 npk, IRailgun.ShieldCiphertext memory c) = note(seed);
        uint64 nonce = deposits.rescueNonces(from);
        uint64 deadline = uint64(block.timestamp + 10 minutes);
        bytes memory sig = rescueSig(i, npk, c, signedFee, nonce, deadline);
        signedFor[from][noteKey(npk, c)] = true;

        alteration %= 4;
        uint128 sentFee = alteration == 1 ? signedFee + 1 : signedFee;
        bytes32 sentNpk = alteration == 2 ? keccak256(abi.encode(npk)) : npk;
        vm.prank(alteration == 3 ? copier : relayer);
        try deposits.rescue(from, sentNpk, c, sentFee, nonce, deadline, sig) {
            if (alteration != 0) alteredLanded = true;
            fees += signedFee;
        } catch {
            if (alteration == 0) honestFailed = true;
        }
    }

    function wait(uint32 duration) external {
        vm.warp(block.timestamp + bound(duration, 0, 2 hours));
    }

    function depositorCount() external pure returns (uint256) {
        return DEPOSITORS;
    }

    /// A note and the hash of its ciphertext, as `MockRailgun` records them.
    function noteKey(bytes32 npk, IRailgun.ShieldCiphertext memory c) public pure returns (bytes32) {
        return keccak256(abi.encode(npk, keccak256(abi.encode(c))));
    }

    function mint(address to, uint256 amount) internal {
        usdc.mint(to, amount);
        minted += amount;
    }

    function note(uint256 seed) internal pure returns (bytes32 npk, IRailgun.ShieldCiphertext memory c) {
        npk = keccak256(abi.encode(seed, "npk"));
        for (uint256 i; i < 3; ++i) {
            c.encryptedBundle[i] = keccak256(abi.encode(seed, i));
        }
        c.shieldKey = keccak256(abi.encode(seed, "shield key"));
    }

    function receiveDigest(address from, uint256 value, uint256 validBefore, bytes32 nonce)
        internal
        view
        returns (bytes32)
    {
        bytes32 structHash = keccak256(
            abi.encode(
                usdc.RECEIVE_WITH_AUTHORIZATION_TYPEHASH(),
                from,
                address(deposits),
                value,
                0,
                validBefore,
                nonce
            )
        );
        return keccak256(abi.encodePacked("\x19\x01", usdc.DOMAIN_SEPARATOR(), structHash));
    }

    function rescueSig(
        uint256 i,
        bytes32 npk,
        IRailgun.ShieldCiphertext memory c,
        uint128 fee,
        uint64 nonce,
        uint64 deadline
    ) internal view returns (bytes memory) {
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
        bytes32 structHash = keccak256(
            abi.encode(
                RESCUE_TYPEHASH, depositors[i], keccak256(abi.encode(npk, c)), relayer, fee, nonce, deadline
            )
        );
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(keys[i], keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, s, v);
    }
}

contract DepositsInvariantTest is Test {
    Deposits internal deposits;
    TestToken internal usdc;
    MockRailgun internal railgun;
    DepositsHandler internal handler;

    function setUp() public {
        usdc = new TestToken();
        railgun = new MockRailgun();
        deposits = new Deposits(address(usdc), railgun, ISanctions(address(0)));
        handler = new DepositsHandler(deposits, usdc, railgun);
        targetContract(address(handler));
    }

    function invariant_theContractHoldsNoTokens() public view {
        assertEq(usdc.balanceOf(address(deposits)), 0);
    }

    /// Into Railgun from a depositor's own vault, to a note that depositor signed for, or to the
    /// relayer the signatures name, for the fees they name: nowhere else.
    function invariant_tokensLeaveOnlyToTheSignedNoteOrTheBoundRelayer() public view {
        assertFalse(handler.alteredLanded(), "an altered request landed");
        assertFalse(handler.honestFailed(), "a request sent as signed failed");
        assertEq(usdc.balanceOf(handler.relayer()), handler.fees());
        assertEq(usdc.balanceOf(handler.copier()), 0);
        for (uint256 i; i < railgun.count(); ++i) {
            (address from, bytes32 npk,,, bytes32 ciphertextHash) = railgun.shielded(i);
            address owner = handler.ownerOf(from);
            assertNotEq(owner, address(0), "shielded from no depositor's vault");
            assertTrue(
                handler.signedFor(owner, keccak256(abi.encode(npk, ciphertextHash))), "an unsigned note"
            );
        }
        uint256 held = usdc.balanceOf(address(railgun)) + usdc.balanceOf(railgun.treasury())
            + usdc.balanceOf(handler.relayer());
        for (uint256 i; i < handler.depositorCount(); ++i) {
            address depositor = handler.depositors(i);
            held += usdc.balanceOf(depositor) + usdc.balanceOf(deposits.vaultOf(depositor));
        }
        assertEq(held, handler.minted());
    }
}
