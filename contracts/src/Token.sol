// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

interface IERC20 {
    function balanceOf(address owner) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @notice ERC-20 calls that accept tokens returning nothing, and fail on any other refusal.
library Token {
    error TransferFailed();

    function transfer(address token, address to, uint256 amount) internal {
        call(token, abi.encodeCall(IERC20.transfer, (to, amount)));
    }

    function transferFrom(address token, address from, address to, uint256 amount) internal {
        call(token, abi.encodeCall(IERC20.transferFrom, (from, to, amount)));
    }

    function approve(address token, address spender, uint256 amount) internal {
        call(token, abi.encodeCall(IERC20.approve, (spender, amount)));
    }

    /// @dev What `owner` holds; an address without code fails as a transfer from it would.
    function balanceOf(address token, address owner) internal view returns (uint256) {
        if (token.code.length == 0) revert TransferFailed();
        return IERC20(token).balanceOf(owner);
    }

    function call(address token, bytes memory data) private {
        (bool ok, bytes memory result) = token.call(data);
        if (!ok || (result.length == 0 ? token.code.length == 0 : !abi.decode(result, (bool)))) {
            revert TransferFailed();
        }
    }
}
