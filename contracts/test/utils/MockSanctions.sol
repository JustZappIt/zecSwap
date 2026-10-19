// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ISanctions} from "../../src/Deposits.sol";

/// Chainalysis' oracle as far as deposits see it: whether an address is sanctioned.
contract MockSanctions is ISanctions {
    mapping(address => bool) public override isSanctioned;

    function sanction(address account) external {
        isSanctioned[account] = true;
    }
}
