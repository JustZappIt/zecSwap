// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";

/// @dev `[k]·SpendAuthG` computed by zecswap-core (`cargo run --example evm_vectors`).
abstract contract SpendAuthVectors is Test {
    struct Vector {
        uint256 k;
        uint256 x;
        uint256 y;
    }

    /// Index of the first seeded-random vector, after q-1, q-2 and the 255 powers of two.
    uint256 internal constant FIRST_RANDOM = 257;
    uint256 internal constant RANDOM_COUNT = 256;

    Vector[] internal vectors;

    function setUp() public virtual {
        string memory json = vm.readFile(string.concat(vm.projectRoot(), "/test/vectors/spend_auth_g.json"));
        Vector[] memory loaded = abi.decode(vm.parseJson(json, ".vectors"), (Vector[]));
        for (uint256 i; i < loaded.length; ++i) {
            vectors.push(loaded[i]);
        }
    }

    function randomVector(uint256 i) internal view returns (Vector memory) {
        return vectors[FIRST_RANDOM + i % RANDOM_COUNT];
    }
}
