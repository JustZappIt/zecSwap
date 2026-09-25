// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IERC20, Token} from "./Token.sol";

/// @notice The part of Railgun's `RailgunSmartWallet` a payout uses.
interface IRailgun {
    struct TokenData {
        uint8 tokenType; // 0: ERC-20
        address tokenAddress;
        uint256 tokenSubID;
    }

    struct CommitmentPreimage {
        bytes32 npk;
        TokenData token;
        uint120 value;
    }

    /// @dev Encrypts only the note's `random`, to the receiver: the token and value it pays are
    /// public and bound by the commitment, not the ciphertext.
    struct ShieldCiphertext {
        bytes32[3] encryptedBundle;
        bytes32 shieldKey;
    }

    struct ShieldRequest {
        CommitmentPreimage preimage;
        ShieldCiphertext ciphertext;
    }

    /// @dev Pulls each request's value from the caller, the fee included.
    function shield(ShieldRequest[] calldata requests) external;
}

/// @title ShieldVault
/// @notice Shields one swap's payout into Railgun. Every shielded payout leaves from its own vault
/// because Railgun's wallets let a note that its screening holds or rejects go only back to the
/// address it was shielded from: a note sent back lands here, tied to its swap, where the swap's
/// user can shield it again. The escrow deploys this once and each vault is a minimal proxy to it,
/// so `ESCROW` is the escrow in every vault.
contract ShieldVault {
    error NotEscrow();
    error ValueTooLarge();

    address private immutable ESCROW = msg.sender;

    /// @notice Pays `fee` of this vault's `token` to `feeTo` and shields the rest to the note
    /// `npk`, which `ciphertext` lets its receiver find.
    function shield(
        IRailgun railgun,
        address token,
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint256 fee,
        address feeTo
    ) external {
        if (msg.sender != ESCROW) revert NotEscrow();
        uint256 value = IERC20(token).balanceOf(address(this)) - fee;
        if (value > type(uint120).max) revert ValueTooLarge();
        if (fee > 0) Token.transfer(token, feeTo, fee);
        Token.approve(token, address(railgun), value);

        IRailgun.ShieldRequest[] memory requests = new IRailgun.ShieldRequest[](1);
        requests[0] = IRailgun.ShieldRequest({
            preimage: IRailgun.CommitmentPreimage({
                npk: npk,
                token: IRailgun.TokenData({tokenType: 0, tokenAddress: token, tokenSubID: 0}),
                value: uint120(value)
            }),
            ciphertext: ciphertext
        });
        railgun.shield(requests);
    }
}
