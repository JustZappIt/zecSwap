// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {HegotaSupport, Devnet, TxParam} from "@forshtat/hegota-support/contracts/HegotaSupport.sol";

/// @notice Reports what the devnet's TXPARAM answers inside a frame transaction: a DEFAULT frame
/// calls it, and it logs every id, its caller, and whether each read succeeded.
contract Probe is HegotaSupport(Devnet.ERC8421_WORKSHOP) {
    event Param(uint256 indexed id, bool ok, uint256 value);
    event Caller(address caller);

    function read(uint256 id) external view returns (uint256) {
        return txparam(TxParam(id));
    }

    fallback() external {
        emit Caller(msg.sender);
        for (uint256 id; id <= uint256(type(TxParam).max); ++id) {
            // Bounded: an id the devnet halts on burns what it is given.
            (bool ok, bytes memory out) =
                address(this).staticcall{gas: 50_000}(abi.encodeCall(this.read, (id)));
            emit Param(id, ok, ok && out.length == 32 ? abi.decode(out, (uint256)) : 0);
        }
    }
}
