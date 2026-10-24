// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";

import {IRailgun} from "../../src/ShieldVault.sol";

interface IRailgunTree {
    function treeNumber() external view returns (uint256);
    function nextLeafIndex() external view returns (uint256);
    function shieldFee() external view returns (uint120);
}

/// The real Railgun and USDC on an Ethereum mainnet fork, and the note zecswap-railgun built in
/// `vectors/railgun_note.json`. Tests skip unless ETH_RPC_URL is set.
abstract contract RailgunFixture is Test {
    address internal constant RAILGUN = 0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9;
    address internal constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    /// 2026-09-25; pinned so forks are cached and repeatable.
    uint256 internal constant FORK_BLOCK = 26_056_542;
    bytes32 internal constant SHIELD_EVENT = keccak256(
        "Shield(uint256,uint256,(bytes32,(uint8,address,uint256),uint120)[],(bytes32[3],bytes32)[],uint256[])"
    );

    bytes32 internal npk;
    IRailgun.ShieldCiphertext internal ciphertext;
    bytes32 internal commitment;

    /// Forks mainnet at `FORK_BLOCK`, or skips the test, and reads the note.
    function forkMainnet() internal {
        string memory rpc = vm.envOr("ETH_RPC_URL", string(""));
        vm.skip(bytes(rpc).length == 0);
        vm.createSelectFork(rpc, FORK_BLOCK);

        string memory json = vm.readFile(string.concat(vm.projectRoot(), "/test/vectors/railgun_note.json"));
        npk = vm.parseJsonBytes32(json, ".npk");
        bytes32[] memory bundle = vm.parseJsonBytes32Array(json, ".encryptedBundle");
        ciphertext.encryptedBundle = [bundle[0], bundle[1], bundle[2]];
        ciphertext.shieldKey = vm.parseJsonBytes32(json, ".shieldKey");
        commitment = vm.parseJsonBytes32(json, ".commitment");
    }

    /// The one `Shield` Railgun emitted since `vm.recordLogs`.
    function shieldEvent()
        internal
        view
        returns (
            IRailgun.CommitmentPreimage[] memory,
            IRailgun.ShieldCiphertext[] memory,
            uint256[] memory fees
        )
    {
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == RAILGUN && logs[i].topics[0] == SHIELD_EVENT) {
                (
                    ,
                    ,
                    IRailgun.CommitmentPreimage[] memory notes,
                    IRailgun.ShieldCiphertext[] memory ciphertexts,
                    uint256[] memory f
                ) = abi.decode(
                    logs[i].data,
                    (uint256, uint256, IRailgun.CommitmentPreimage[], IRailgun.ShieldCiphertext[], uint256[])
                );
                return (notes, ciphertexts, f);
            }
        }
        revert("no Shield event");
    }

    function leafCount() internal view returns (uint256) {
        return IRailgunTree(RAILGUN).treeNumber() * 2 ** 16 + IRailgunTree(RAILGUN).nextLeafIndex();
    }
}
