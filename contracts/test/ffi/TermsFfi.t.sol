// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IRailgun} from "../../src/ShieldVault.sol";
import {ZecSwap} from "../../src/ZecSwap.sol";
import {SpendAuthVectors} from "../utils/SpendAuthVectors.sol";

/// zecswap-core's `Terms::hash` against `hashTerms`, on random terms; see the `ffi` profile in
/// foundry.toml. Wallets and relayers check swaps with the Rust hash.
contract TermsFfiTest is SpendAuthVectors {
    ZecSwap internal swaps;

    function setUp() public override {
        vm.skip(keccak256(bytes(vm.envOr("FOUNDRY_PROFILE", string("")))) != keccak256("ffi"));
        super.setUp();
        swaps = new ZecSwap(2 hours, IRailgun(address(0)));
    }

    function testFuzz_rustHashesTermsAsTheContractDoes(
        address maker,
        address token,
        uint128 amount,
        uint256 makerIndex,
        uint256 userIndex,
        address user,
        uint64 t0,
        uint64 t1,
        bytes32 payoutNote
    ) public {
        Vector memory makerShare = randomVector(makerIndex);
        Vector memory userShare = randomVector(userIndex);
        ZecSwap.Terms memory terms = ZecSwap.Terms(
            maker,
            token,
            amount,
            [makerShare.x, makerShare.y],
            [userShare.x, userShare.y],
            user,
            t0,
            t1,
            payoutNote
        );
        assertEq(rustHash(terms), swaps.hashTerms(terms));
    }

    function rustHash(ZecSwap.Terms memory terms) internal returns (bytes32) {
        string[] memory cmd = new string[](11);
        cmd[0] = string.concat(vm.projectRoot(), "/../target/release/examples/evm_vectors");
        cmd[1] = "terms";
        cmd[2] = vm.toString(terms.maker);
        cmd[3] = vm.toString(terms.token);
        cmd[4] = vm.toString(terms.amount);
        cmd[5] = vm.toString(abi.encodePacked(terms.makerKey));
        cmd[6] = vm.toString(abi.encodePacked(terms.userKey));
        cmd[7] = vm.toString(terms.user);
        cmd[8] = vm.toString(terms.t0);
        cmd[9] = vm.toString(terms.t1);
        cmd[10] = vm.toString(terms.payoutNote);
        return abi.decode(vm.ffi(cmd), (bytes32));
    }
}
