// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {console} from "forge-std/Test.sol";

import {Deposits, ISanctions} from "../../src/Deposits.sol";
import {IRailgun} from "../../src/ShieldVault.sol";
import {IERC20} from "../../src/Token.sol";
import {IRailgunTree, RailgunFixture} from "./RailgunFixture.sol";

interface IUsdc {
    function name() external view returns (string memory);
    function version() external view returns (string memory);
    function DOMAIN_SEPARATOR() external view returns (bytes32);
    function authorizationState(address authorizer, bytes32 nonce) external view returns (bool);
}

/// Deposits against the real USDC, Railgun and sanctions oracle on an Ethereum mainnet fork, paying
/// the note zecswap-railgun built. Its gas is what the relayer's deposit fee pays for:
///   ETH_RPC_URL=… forge test --match-path test/fork/DepositsFork.t.sol --isolate -vv
contract DepositsForkTest is RailgunFixture {
    address internal constant SANCTIONS = 0x40C57923924B5c5c5455c48D93317139ADDaC8fb;
    /// On OFAC's list, and flagged by the oracle at the fork block.
    address internal constant SANCTIONED = 0x7F367cC41522cE07553e823bf3be79A889DEbe1B;
    bytes32 internal constant RECEIVE_WITH_AUTHORIZATION_TYPEHASH = keccak256(
        "ReceiveWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)"
    );
    bytes32 internal constant RESCUE_TYPEHASH = keccak256(
        "Rescue(address from,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)"
    );
    bytes32 internal constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    uint256 internal constant VALUE = 500e6;
    uint128 internal constant FEE = 600_000;

    Deposits internal deposits;
    address internal relayer = makeAddr("relayer");
    address internal depositor;
    uint256 internal depositorKey;

    function setUp() public {
        forkMainnet();
        deposits = new Deposits(USDC, IRailgun(RAILGUN), ISanctions(SANCTIONS));
        (depositor, depositorKey) = makeAddrAndKey("depositor");
    }

    /// Wallets build USDC's domain from its `name()` and `version()`, and check it against
    /// `DOMAIN_SEPARATOR()`.
    function test_usdcsDomainIsItsNameAndVersion() public view {
        string memory name = IUsdc(USDC).name();
        string memory version = IUsdc(USDC).version();
        assertEq(name, "USD Coin");
        assertEq(version, "2");
        assertEq(
            keccak256(abi.encode(DOMAIN_TYPEHASH, keccak256(bytes(name)), keccak256(bytes(version)), 1, USDC)),
            IUsdc(USDC).DOMAIN_SEPARATOR()
        );
    }

    function test_rustAndSolidityCommitToTheSameNote() public view {
        assertEq(deposits.noteCommitment(npk, ciphertext), commitment);
    }

    /// A first deposit from an address makes its vault; a later one goes through it.
    function test_aSignedAuthorizationShieldsItsValueLessTheFees() public {
        address vault = deposits.vaultOf(depositor);
        uint256 firstGas = deposit(FEE);
        assertGt(vault.code.length, 0);
        uint256 laterGas = deposit(FEE + 1);
        assertEq(IERC20(USDC).balanceOf(relayer), 2 * FEE + 1);
        assertEq(IERC20(USDC).balanceOf(address(deposits)), 0);
        assertEq(IERC20(USDC).balanceOf(vault), 0);
        console.log("first deposit gas", firstGas);
        console.log("later deposit gas", laterGas);
    }

    /// Refused before anything else is read: USDC has blacklisted it too.
    function test_aSanctionedDepositorIsRefused() public {
        uint256 validBefore = block.timestamp + 1 hours;
        vm.prank(relayer);
        vm.expectRevert(Deposits.Sanctioned.selector);
        deposits.shield(SANCTIONED, VALUE, 0, validBefore, npk, ciphertext, FEE, 27, bytes32(0), bytes32(0));
    }

    function test_rescueShieldsWhatCameBackAgain() public {
        deposit(FEE);
        address vault = deposits.vaultOf(depositor);
        deal(USDC, vault, 400e6);
        uint64 deadline = uint64(block.timestamp + 10 minutes);
        bytes memory sig = rescueSig(deadline);
        uint256 leaves = leafCount();
        vm.recordLogs();
        vm.prank(relayer);
        uint256 gas = gasleft();
        deposits.rescue(depositor, npk, ciphertext, FEE, 0, deadline, sig);
        gas -= gasleft();
        (IRailgun.CommitmentPreimage[] memory notes,,) = shieldEvent();
        assertEq(notes[0].npk, npk);
        assertEq(leafCount(), leaves + 1);
        assertEq(IERC20(USDC).balanceOf(vault), 0);
        console.log("rescue gas", gas);
    }

    /// The depositor's signature of `VALUE` to `deposits`, valid for an hour.
    struct Signed {
        uint256 validBefore;
        bytes32 nonce;
        uint8 v;
        bytes32 r;
        bytes32 s;
    }

    /// Mints `VALUE` to the depositor and shields it for `fee`, checking what Railgun recorded;
    /// the shield's gas.
    function deposit(uint128 fee) internal returns (uint256 gas) {
        deal(USDC, depositor, VALUE);
        uint256 relayed = IERC20(USDC).balanceOf(relayer);
        uint256 leaves = leafCount();
        Signed memory signed = sign(fee);
        vm.recordLogs();
        vm.prank(relayer);
        gas = gasleft();
        deposits.shield(
            depositor, VALUE, 0, signed.validBefore, npk, ciphertext, fee, signed.v, signed.r, signed.s
        );
        gas -= gasleft();

        checkShielded(fee);
        assertEq(leafCount(), leaves + 1);
        assertEq(IERC20(USDC).balanceOf(relayer), relayed + fee);
        assertEq(IERC20(USDC).balanceOf(depositor), 0);
        assertTrue(IUsdc(USDC).authorizationState(depositor, signed.nonce));
    }

    /// The one note Railgun shielded since `vm.recordLogs`: `VALUE` less `fee` and Railgun's fee.
    function checkShielded(uint128 fee) internal view {
        (
            IRailgun.CommitmentPreimage[] memory notes,
            IRailgun.ShieldCiphertext[] memory ciphertexts,
            uint256[] memory fees
        ) = shieldEvent();
        uint120 value = uint120(VALUE - fee);
        uint120 railgunFee = value * IRailgunTree(RAILGUN).shieldFee() / 10_000;
        assertEq(notes.length, 1);
        assertEq(notes[0].npk, npk);
        assertEq(notes[0].token.tokenType, 0);
        assertEq(notes[0].token.tokenAddress, USDC);
        assertEq(notes[0].value, value - railgunFee);
        assertEq(keccak256(abi.encode(ciphertexts[0])), keccak256(abi.encode(ciphertext)));
        assertEq(fees[0], railgunFee);
    }

    function sign(uint128 fee) internal view returns (Signed memory signed) {
        signed.validBefore = block.timestamp + 1 hours;
        signed.nonce = deposits.depositNonce(npk, ciphertext, fee, relayer);
        (signed.v, signed.r, signed.s) =
            vm.sign(depositorKey, receiveDigest(signed.validBefore, signed.nonce));
    }

    function receiveDigest(uint256 validBefore, bytes32 nonce) internal view returns (bytes32) {
        bytes32 structHash = keccak256(
            abi.encode(
                RECEIVE_WITH_AUTHORIZATION_TYPEHASH,
                depositor,
                address(deposits),
                VALUE,
                0,
                validBefore,
                nonce
            )
        );
        return keccak256(abi.encodePacked("\x19\x01", IUsdc(USDC).DOMAIN_SEPARATOR(), structHash));
    }

    function rescueSig(uint64 deadline) internal view returns (bytes memory) {
        bytes32 domain = keccak256(
            abi.encode(
                DOMAIN_TYPEHASH,
                keccak256("ZecSwap Deposits"),
                keccak256("1"),
                block.chainid,
                address(deposits)
            )
        );
        bytes32 structHash =
            keccak256(abi.encode(RESCUE_TYPEHASH, depositor, commitment, relayer, FEE, uint64(0), deadline));
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(depositorKey, keccak256(abi.encodePacked("\x19\x01", domain, structHash)));
        return abi.encodePacked(r, s, v);
    }
}
