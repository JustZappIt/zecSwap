// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IRailgun} from "../../src/ShieldVault.sol";
import {IERC20} from "../../src/Token.sol";

/// Railgun's `shield` as far as a payout sees it: the fee comes out of the value, both parts are
/// pulled from the caller, and each note is recorded and emitted as Railgun emits it.
contract MockRailgun is IRailgun {
    event Shield(
        uint256 treeNumber,
        uint256 startPosition,
        CommitmentPreimage[] commitments,
        ShieldCiphertext[] shieldCiphertext,
        uint256[] fees
    );

    uint120 public constant FEE_BPS = 25;

    struct Shielded {
        address from;
        bytes32 npk;
        address token;
        uint120 value;
        bytes32 ciphertextHash;
    }

    address public immutable treasury = address(0x7ea5);
    Shielded[] public shielded;
    bool public paused;
    mapping(address token => bool) public tokenBlocklist;

    function setPaused(bool paused_) external {
        paused = paused_;
    }

    function setTokenBlocked(address token, bool blocked) external {
        tokenBlocklist[token] = blocked;
    }

    function shield(ShieldRequest[] calldata requests) external {
        require(!paused, "paused");
        for (uint256 i; i < requests.length; ++i) {
            CommitmentPreimage calldata preimage = requests[i].preimage;
            require(preimage.value > 0, "Invalid Note Value");
            uint120 fee = preimage.value * FEE_BPS / 10_000;
            uint120 base = preimage.value - fee;
            IERC20 token = IERC20(preimage.token.tokenAddress);
            require(token.transferFrom(msg.sender, address(this), base), "transfer");
            require(token.transferFrom(msg.sender, treasury, fee), "fee");
            CommitmentPreimage[] memory commitments = new CommitmentPreimage[](1);
            commitments[0] = CommitmentPreimage(preimage.npk, preimage.token, base);
            ShieldCiphertext[] memory ciphertexts = new ShieldCiphertext[](1);
            ciphertexts[0] = requests[i].ciphertext;
            uint256[] memory fees = new uint256[](1);
            fees[0] = fee;
            emit Shield(0, shielded.length, commitments, ciphertexts, fees);
            shielded.push(
                Shielded(
                    msg.sender,
                    preimage.npk,
                    address(token),
                    base,
                    keccak256(abi.encode(requests[i].ciphertext))
                )
            );
        }
    }

    function count() external view returns (uint256) {
        return shielded.length;
    }

    function last() external view returns (Shielded memory) {
        return shielded[shielded.length - 1];
    }
}
