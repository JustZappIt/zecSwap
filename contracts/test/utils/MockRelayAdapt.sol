// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {TestToken} from "./TestToken.sol";

/// Test-only V2 ABI harness. Mints in place of an unshield and checks a dummy proof marker.
/// Real SNARK verification and deployed Relay Adapt compatibility need a live Railgun test.
contract MockRelayAdapt {
    struct G1Point {
        uint256 x;
        uint256 y;
    }

    struct G2Point {
        uint256[2] x;
        uint256[2] y;
    }

    struct SnarkProof {
        G1Point a;
        G2Point b;
        G1Point c;
    }

    struct CommitmentCiphertext {
        bytes32[4] ciphertext;
        bytes32 blindedSenderViewingKey;
        bytes32 blindedReceiverViewingKey;
        bytes annotationData;
        bytes memo;
    }

    struct BoundParams {
        uint16 treeNumber;
        uint72 minGasPrice;
        uint8 unshield;
        uint64 chainID;
        address adaptContract;
        bytes32 adaptParams;
        CommitmentCiphertext[] commitmentCiphertext;
    }

    struct TokenData {
        uint8 tokenType;
        address tokenAddress;
        uint256 tokenSubID;
    }

    struct Preimage {
        bytes32 npk;
        TokenData token;
        uint120 value;
    }

    struct Transaction {
        SnarkProof proof;
        bytes32 merkleRoot;
        bytes32[] nullifiers;
        bytes32[] commitments;
        BoundParams boundParams;
        Preimage unshieldPreimage;
    }

    struct Call {
        address to;
        bytes data;
        uint256 value;
    }

    struct ActionData {
        bytes31 random;
        bool requireSuccess;
        uint256 minGasLimit;
        Call[] calls;
    }

    struct Ciphertext {
        bytes32[3] encryptedBundle;
        bytes32 shieldKey;
    }

    struct ShieldRequest {
        Preimage preimage;
        Ciphertext ciphertext;
    }

    address public immutable railgun;
    mapping(bytes32 => bool) public spent;

    constructor(address railgun_) {
        railgun = railgun_;
    }

    function relay(Transaction[] calldata transactions, ActionData calldata action) external payable {
        require(action.requireSuccess, "atomic calls required");
        require(gasleft() >= action.minGasLimit, "gas");
        bytes32[][] memory nullifiers = new bytes32[][](transactions.length);
        for (uint256 i; i < transactions.length; ++i) {
            nullifiers[i] = transactions[i].nullifiers;
        }
        bytes32 params = keccak256(abi.encode(nullifiers, transactions.length, action));
        for (uint256 i; i < transactions.length; ++i) {
            Transaction calldata txn = transactions[i];
            require(txn.proof.a.x == 1, "invalid dummy proof");
            require(txn.boundParams.adaptParams == params, "binding");
            require(txn.boundParams.chainID == block.chainid, "chain");
            require(txn.boundParams.adaptContract == address(this), "adapter");
            for (uint256 j; j < txn.nullifiers.length; ++j) {
                require(!spent[txn.nullifiers[j]], "spent");
                spent[txn.nullifiers[j]] = true;
            }
            if (txn.boundParams.unshield == 1) {
                TestToken(txn.unshieldPreimage.token.tokenAddress).mint(
                    address(this), txn.unshieldPreimage.value
                );
            }
        }
        for (uint256 i; i < action.calls.length; ++i) {
            Call calldata call = action.calls[i];
            (bool ok,) = call.to.call{value: call.value}(call.data);
            require(ok, "call failed");
        }
    }

    function shield(ShieldRequest[] calldata requests) external view {
        for (uint256 i; i < requests.length; ++i) {
            require(
                TestToken(requests[i].preimage.token.tokenAddress).balanceOf(address(this)) == 0,
                "unexpected dust in harness"
            );
        }
    }
}
