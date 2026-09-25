// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Pallas} from "../src/Pallas.sol";
import {PallasHarness} from "./utils/PallasHarness.sol";
import {SpendAuthVectors} from "./utils/SpendAuthVectors.sol";

contract PallasTest is SpendAuthVectors {
    PallasHarness internal pallas = new PallasHarness();

    function test_matchesRustVectors() public view {
        assertEq(vectors.length, FIRST_RANDOM + RANDOM_COUNT);
        for (uint256 i; i < vectors.length; ++i) {
            Vector memory v = vectors[i];
            assertTrue(pallas.isOnCurve(v.x, v.y), "vector point on curve");
            assertTrue(pallas.isSpendAuthMul(v.k, v.x, v.y), "vector matches");
        }
    }

    /// [q - k]·G = -[k]·G: an independent check of each vector through a different bit pattern.
    function test_negatedScalarGivesNegatedPoint() public view {
        for (uint256 i; i < vectors.length; ++i) {
            Vector memory v = vectors[i];
            assertTrue(pallas.isSpendAuthMul(Pallas.Q - v.k, v.x, Pallas.P - v.y));
            assertFalse(pallas.isSpendAuthMul(v.k, v.x, Pallas.P - v.y));
        }
    }

    function test_rejectsNeighbouringScalars() public view {
        for (uint256 i; i < RANDOM_COUNT; ++i) {
            Vector memory v = randomVector(i);
            assertFalse(pallas.isSpendAuthMul(v.k + 1, v.x, v.y));
            assertFalse(pallas.isSpendAuthMul(v.k - 1, v.x, v.y));
        }
    }

    function test_rejectsScalarsOutsideTheGroupOrder() public view {
        assertFalse(pallas.isSpendAuthMul(0, Pallas.GX, Pallas.GY));
        assertFalse(pallas.isSpendAuthMul(Pallas.Q, Pallas.GX, Pallas.GY));
        // q + 1 reduces to 1, so an unchecked implementation would accept it.
        assertFalse(pallas.isSpendAuthMul(Pallas.Q + 1, Pallas.GX, Pallas.GY));
        assertFalse(pallas.isSpendAuthMul(type(uint256).max, Pallas.GX, Pallas.GY));
    }

    function test_rejectsNonCanonicalCoordinates() public view {
        assertFalse(pallas.isSpendAuthMul(1, Pallas.GX + Pallas.P, Pallas.GY));
        assertFalse(pallas.isSpendAuthMul(1, Pallas.GX, Pallas.GY + Pallas.P));
        assertFalse(pallas.isOnCurve(Pallas.GX + Pallas.P, Pallas.GY));
    }

    function test_isOnCurve() public view {
        assertTrue(pallas.isOnCurve(Pallas.GX, Pallas.GY));
        assertTrue(pallas.isOnCurve(Pallas.GX, Pallas.P - Pallas.GY));
        assertFalse(pallas.isOnCurve(0, 0));
        assertFalse(pallas.isOnCurve(Pallas.GX, Pallas.GY + 1));
    }

    function testFuzz_rejectsScalarsForOtherPoints(uint256 seed, uint256 k) public view {
        Vector memory v = randomVector(seed);
        k = bound(k, 1, Pallas.Q - 1);
        vm.assume(k != v.k);
        assertFalse(pallas.isSpendAuthMul(k, v.x, v.y));
    }

    function test_gas_fullWidthScalar() public view {
        Vector memory v = randomVector(0);
        uint256 start = gasleft();
        pallas.isSpendAuthMul(v.k, v.x, v.y);
        assertLt(start - gasleft(), 200_000);
    }
}
