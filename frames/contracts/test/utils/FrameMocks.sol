// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {IStakingRegistry} from "../../src/ClaimSponsor.sol";

/// Forge's EVM has no frame opcodes, so these stand in for the helpers a `HegotaSupport` contract
/// creates in its constructor, etched where they landed. On the frames devnet APPROVE is the helper
/// created first (nonce 1) and TXPARAM the second (nonce 2).
library FrameHelpers {
    uint256 internal constant APPROVE_NONCE = 1;
    uint256 internal constant TXPARAM_NONCE = 2;
    /// One per opcode the devnet has: a contract inheriting `HegotaSupport` starts at this nonce.
    uint256 internal constant NEXT_NONCE = 8;
}

/// APPROVE, run by delegatecall: logs the scope from the approving contract.
contract MockApprove {
    event Approved(uint256 scope);

    fallback() external payable {
        emit Approved(abi.decode(msg.data, (uint256)));
    }
}

/// TXPARAM, answering whatever a test set for each parameter id.
contract MockTxParam {
    mapping(uint256 id => uint256) public answer;

    function set(uint256 id, uint256 value) external {
        answer[id] = value;
    }

    fallback(bytes calldata id) external returns (bytes memory) {
        return abi.encode(answer[abi.decode(id, (uint256))]);
    }
}

/// The devnet's staking registry as far as a sponsor sees it.
contract MockRegistry is IStakingRegistry {
    mapping(address staker => uint256) public stakeOf;
    mapping(address staker => uint32) public delayOf;

    function addStake(uint32 unstakeDelaySec) external payable {
        stakeOf[msg.sender] += msg.value;
        delayOf[msg.sender] = unstakeDelaySec;
    }
}
