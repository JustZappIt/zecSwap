// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

import {Pallas} from "../../src/Pallas.sol";
import {PallasHarness} from "../utils/PallasHarness.sol";

/// Differential fuzzing against zecswap-core; see the `ffi` profile in foundry.toml.
contract PallasFfiTest is Test {
    PallasHarness internal pallas = new PallasHarness();

    function setUp() public {
        vm.skip(keccak256(bytes(vm.envOr("FOUNDRY_PROFILE", string("")))) != keccak256("ffi"));
    }

    function testFuzz_matchesRust(uint256 k) public {
        k = bound(k, 1, Pallas.Q - 1);
        string[] memory cmd = new string[](3);
        cmd[0] = string.concat(vm.projectRoot(), "/../target/release/examples/evm_vectors");
        cmd[1] = "mul";
        cmd[2] = vm.toString(bytes32(k));
        (uint256 x, uint256 y) = abi.decode(vm.ffi(cmd), (uint256, uint256));
        assertTrue(pallas.isSpendAuthMul(k, x, y));
        assertFalse(pallas.isSpendAuthMul(k, x, Pallas.P - y));
    }
}
