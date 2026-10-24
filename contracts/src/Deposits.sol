// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {IRailgun, ShieldVault} from "./ShieldVault.sol";
import {Token} from "./Token.sol";

/// @notice EIP-3009's pull, as USDC has it: only `to` may submit the authorization.
interface IReceiveWithAuthorization {
    function receiveWithAuthorization(
        address from,
        address to,
        uint256 value,
        uint256 validAfter,
        uint256 validBefore,
        bytes32 nonce,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external;
}

/// @notice Chainalysis' sanctions oracle.
interface ISanctions {
    function isSanctioned(address addr) external view returns (bool);
}

/// @title Deposits
/// @notice Shields public USDC into its owner's private Railgun balance, for wallets that hold no
/// ETH. The owner signs one EIP-3009 `receiveWithAuthorization` of its deposit address's USDC to
/// this contract, and a relayer sends it: the USDC moves into the address's own vault, which pays
/// the relayer its fee and shields the rest to the note the owner built. The authorization's nonce
/// commits to the note, the fee and the relayer, so nobody can change them, and nobody but that
/// relayer can land the authorization.
/// @dev Railgun's wallets send a note its screening refuses back only to the address it was
/// shielded from: a vault, one per deposit address, whose owner shields it again with `rescue`.
contract Deposits {
    /// @notice What a deposit's authorization nonce commits to (`depositNonce`).
    bytes32 public constant DEPOSIT_TYPEHASH = keccak256(
        "Deposit(uint256 chainId,address deposits,bytes32 npk,bytes32[3] encryptedBundle,bytes32 shieldKey,uint128 fee,address relayer)"
    );
    bytes32 private constant RESCUE_TYPEHASH = keccak256(
        "Rescue(address from,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)"
    );
    bytes32 private constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    /// secp256k1's n / 2. A signature with a larger `s` is the malleated twin of one without.
    uint256 private constant HALF_N = 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;

    /// @notice The token deposits are in: USDC, or a testnet's stand-in with EIP-3009.
    address public immutable TOKEN;
    IRailgun public immutable RAILGUN;
    /// @notice The sanctions oracle each depositor is checked against; none on testnets.
    ISanctions public immutable SANCTIONS;
    /// @notice The code every vault runs: each is an EIP-1167 proxy to this one.
    ShieldVault public immutable VAULT_LOGIC;
    bytes32 private immutable VAULT_CODE_HASH;

    /// @notice Each depositor's next rescue signature's nonce.
    mapping(address from => uint64) public rescueNonces;

    /// @notice `value` is what the vault shielded, before Railgun's own fee.
    event DepositShielded(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);
    event Rescued(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);

    error ZeroAddress();
    error NoSanctionsOracle();
    error Sanctioned();
    error InvalidFee();
    error ValueTooLarge();
    error Expired();
    error BadSignature();
    error VaultNotDeployed();

    constructor(address token, IRailgun railgun, ISanctions sanctions) {
        if (token == address(0) || address(railgun) == address(0)) revert ZeroAddress();
        // An oracle without code would refuse every deposit.
        bool oracle = address(sanctions).code.length > 0;
        if (address(sanctions) == address(0) ? block.chainid == 1 : !oracle) revert NoSanctionsOracle();
        TOKEN = token;
        RAILGUN = railgun;
        SANCTIONS = sanctions;
        VAULT_LOGIC = new ShieldVault();
        VAULT_CODE_HASH = keccak256(_vaultCode());
    }

    /// @notice Takes `value` of `from`'s token on its authorization, and shields it less `fee`,
    /// which goes to the caller, to the note `npk` that `ciphertext` lets its receiver find.
    function shield(
        address from,
        uint256 value,
        uint256 validAfter,
        uint256 validBefore,
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint128 fee,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external {
        // Railgun notes hold at most uint120.
        if (value > type(uint120).max) revert ValueTooLarge();
        if (fee >= value) revert InvalidFee();
        _checkSanctions(from);
        IReceiveWithAuthorization(TOKEN).receiveWithAuthorization(
            from,
            address(this),
            value,
            validAfter,
            validBefore,
            depositNonce(npk, ciphertext, fee, msg.sender),
            v,
            r,
            s
        );
        address vault = vaultOf(from);
        if (vault.code.length == 0) _deployVault(from);
        Token.transfer(TOKEN, vault, value);
        emit DepositShielded(
            from, _shield(vault, npk, ciphertext, fee), fee, msg.sender, noteCommitment(npk, ciphertext)
        );
    }

    /// @notice Shields what Railgun sent back to `from`'s vault again, to a note `from` signs for,
    /// less the fee it signs for the relayer that sends this.
    function rescue(
        address from,
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint128 fee,
        uint64 nonce,
        uint64 deadline,
        bytes calldata signature
    ) external {
        if (block.timestamp > deadline) revert Expired();
        if (nonce != rescueNonces[from]) revert BadSignature();
        _checkSanctions(from);
        bytes32 note = noteCommitment(npk, ciphertext);
        _checkSignature(
            from,
            keccak256(abi.encode(RESCUE_TYPEHASH, from, note, msg.sender, fee, nonce, deadline)),
            signature
        );
        address vault = vaultOf(from);
        if (vault.code.length == 0) revert VaultNotDeployed();
        rescueNonces[from] = nonce + 1;
        emit Rescued(from, _shield(vault, npk, ciphertext, fee), fee, msg.sender, note);
    }

    /// @notice The nonce of an authorization to shield to the note `npk` and `ciphertext`, paying
    /// `fee` to `relayer`, which alone can send it.
    function depositNonce(
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint128 fee,
        address relayer
    ) public view returns (bytes32) {
        return keccak256(
            abi.encode(DEPOSIT_TYPEHASH, block.chainid, address(this), npk, ciphertext, fee, relayer)
        );
    }

    /// @notice What a note is known by: as `ZecSwap.noteCommitment`.
    function noteCommitment(bytes32 npk, IRailgun.ShieldCiphertext calldata ciphertext)
        public
        pure
        returns (bytes32)
    {
        return keccak256(abi.encode(npk, ciphertext));
    }

    /// @notice Whether `from` has deposited: its vault is there. With its token balance, what a
    /// wallet reads of each of its deposit addresses to find the ones it used, in one Multicall3
    /// call for many.
    function deposited(address from) external view returns (bool) {
        return vaultOf(from).code.length > 0;
    }

    /// @notice Where `from`'s deposits shield from, and where Railgun sends them back.
    function vaultOf(address from) public view returns (address) {
        return address(
            uint160(
                uint256(
                    keccak256(abi.encodePacked(bytes1(0xff), address(this), _salt(from), VAULT_CODE_HASH))
                )
            )
        );
    }

    /// @dev Shields the vault's whole balance less `fee`, which goes to the caller: what Railgun
    /// sent back goes in again, to the same owner.
    function _shield(address vault, bytes32 npk, IRailgun.ShieldCiphertext calldata ciphertext, uint128 fee)
        private
        returns (uint256 value)
    {
        uint256 balance = Token.balanceOf(TOKEN, vault);
        if (fee >= balance) revert InvalidFee();
        ShieldVault(vault).shield(RAILGUN, TOKEN, npk, ciphertext, fee, msg.sender);
        return balance - fee;
    }

    function _deployVault(address from) private {
        bytes memory code = _vaultCode();
        bytes32 salt = _salt(from);
        address vault;
        assembly ("memory-safe") {
            vault := create2(0, add(code, 0x20), mload(code), salt)
        }
        if (vault == address(0)) revert VaultNotDeployed();
    }

    function _checkSanctions(address from) private view {
        if (address(SANCTIONS) != address(0) && SANCTIONS.isSanctioned(from)) revert Sanctioned();
    }

    function _salt(address from) private pure returns (bytes32) {
        return bytes32(uint256(uint160(from)));
    }

    /// @dev EIP-1167 creation code of a minimal proxy to `VAULT_LOGIC`.
    function _vaultCode() private view returns (bytes memory) {
        return abi.encodePacked(
            hex"3d602d80600a3d3981f3363d3d373d3d3d363d73",
            address(VAULT_LOGIC),
            hex"5af43d82803e903d91602b57fd5bf3"
        );
    }

    /// @dev Checks an EIP-712 signature by `signer` over `structHash`, under this contract's
    /// domain on this chain.
    function _checkSignature(address signer, bytes32 structHash, bytes calldata signature) private view {
        bytes32 domain = keccak256(
            abi.encode(
                DOMAIN_TYPEHASH, keccak256("ZecSwap Deposits"), keccak256("1"), block.chainid, address(this)
            )
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", domain, structHash));
        if (signature.length != 65 || uint256(bytes32(signature[32:64])) > HALF_N) revert BadSignature();
        address recovered =
            ecrecover(digest, uint8(signature[64]), bytes32(signature[:32]), bytes32(signature[32:64]));
        if (recovered == address(0) || recovered != signer) revert BadSignature();
    }
}
