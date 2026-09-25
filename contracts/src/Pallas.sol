// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/// @notice Pallas arithmetic for checking revealed Orchard spend-key shares.
/// @dev Pallas is y² = x³ + 5 over F_p with prime order q. The base is Orchard's
/// SpendAuthG = GroupHash("z.cash:Orchard", "G"), not the curve's standard generator.
library Pallas {
    uint256 internal constant P = 0x40000000000000000000000000000000224698fc094cf91b992d30ed00000001;
    uint256 internal constant Q = 0x40000000000000000000000000000000224698fc0994a8dd8c46eb2100000001;
    uint256 internal constant GX = 0x375523b328f1d6063b8d187c3e5f445f0c7f0ce37b70a10c8d1a7284b875c963;
    uint256 internal constant GY = 0x1ad0357fdf1a66db7b10bcfcfed624fbdfc914fec005bdd84ce33e817b0c3bc9;

    /// @notice Whether (x, y) is a canonically encoded affine point. The identity has no
    /// affine form and (0, 0) is not on the curve, so this also rejects it.
    function isOnCurve(uint256 x, uint256 y) internal pure returns (bool) {
        return x < P && y < P && mulmod(y, y, P) == addmod(mulmod(mulmod(x, x, P), x, P), 5, P);
    }

    /// @notice Whether (x, y) == [k]·SpendAuthG, for a scalar 0 < k < q.
    function isSpendAuthMul(uint256 k, uint256 x, uint256 y) internal pure returns (bool) {
        if (k == 0 || k >= Q || x >= P || y >= P) return false;
        (uint256 jx, uint256 jy, uint256 jz) = mulSpendAuthG(k);
        uint256 zz = mulmod(jz, jz, P);
        return jx == mulmod(x, zz, P) && jy == mulmod(y, mulmod(zz, jz, P), P);
    }

    /// @dev Left-to-right double-and-add in Jacobian coordinates, starting from the top set
    /// bit. The accumulator before an addition is [2m]·G with 2m + 1 <= k < q, so it is never
    /// ±G and the incomplete mixed-addition formula is exact; Pallas has no 2-torsion, so
    /// doubling is exact too. Formulas: dbl-2009-l and madd-2007-bl (a = 0).
    function mulSpendAuthG(uint256 k) private pure returns (uint256 x, uint256 y, uint256 z) {
        assembly ("memory-safe") {
            let p := P
            x := GX
            y := GY
            z := 1

            let bit := 254
            for {} iszero(shr(bit, k)) { bit := sub(bit, 1) } {}

            for {} bit {} {
                bit := sub(bit, 1)

                let a := mulmod(x, x, p)
                let b := mulmod(y, y, p)
                let c := mulmod(b, b, p)
                let t := addmod(x, b, p)
                let d := addmod(mulmod(t, t, p), sub(p, addmod(a, c, p)), p)
                d := addmod(d, d, p)
                let e := mulmod(3, a, p)
                z := mulmod(addmod(y, y, p), z, p)
                x := addmod(mulmod(e, e, p), sub(p, addmod(d, d, p)), p)
                y := addmod(mulmod(e, addmod(d, sub(p, x), p), p), sub(p, mulmod(8, c, p)), p)

                if and(shr(bit, k), 1) {
                    let zz := mulmod(z, z, p)
                    let h := addmod(mulmod(GX, zz, p), sub(p, x), p)
                    let r := addmod(mulmod(GY, mulmod(z, zz, p), p), sub(p, y), p)
                    r := addmod(r, r, p)
                    let hh := mulmod(h, h, p)
                    let i := mulmod(4, hh, p)
                    let j := mulmod(h, i, p)
                    let v := mulmod(x, i, p)
                    let s := addmod(z, h, p)
                    z := addmod(mulmod(s, s, p), sub(p, addmod(zz, hh, p)), p)
                    let x3 := addmod(mulmod(r, r, p), sub(p, addmod(j, addmod(v, v, p), p)), p)
                    y :=
                        addmod(mulmod(r, addmod(v, sub(p, x3), p), p), sub(p, mulmod(2, mulmod(y, j, p), p)), p)
                    x := x3
                }
            }
        }
    }
}
