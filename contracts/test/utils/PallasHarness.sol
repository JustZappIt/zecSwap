// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Pallas} from "../../src/Pallas.sol";

contract PallasHarness {
    function isSpendAuthMul(uint256 k, uint256 x, uint256 y) external pure returns (bool) {
        return Pallas.isSpendAuthMul(k, x, y);
    }

    function isOnCurve(uint256 x, uint256 y) external pure returns (bool) {
        return Pallas.isOnCurve(x, y);
    }
}
