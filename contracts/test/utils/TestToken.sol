// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

/// A mintable, pausable payout token for tests and testnets. It behaves as USDC does wherever
/// deposits and Railgun depend on it: ERC-20's events and metadata, and EIP-3009's
/// `receiveWithAuthorization` under an EIP-712 domain of its own name and USDC's version. Railgun's
/// wallets find a returned note's origin from its `Transfer`s.
contract TestToken {
    string public constant name = "ZecSwap Test USD";
    string public constant symbol = "tUSD";
    uint8 public constant decimals = 6;
    /// The EIP-712 domain's version, USDC's.
    string public constant version = "2";

    bytes32 public constant RECEIVE_WITH_AUTHORIZATION_TYPEHASH = keccak256(
        "ReceiveWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)"
    );
    bytes32 private constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    /// secp256k1's n / 2: USDC refuses the malleated twin of a signature, with the larger `s`.
    uint256 private constant HALF_N = 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;

    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    /// Whether `authorizer` has used `nonce`, as EIP-3009 names it.
    mapping(address authorizer => mapping(bytes32 nonce => bool)) public authorizationState;
    bool public paused;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event AuthorizationUsed(address indexed authorizer, bytes32 indexed nonce);

    function setPaused(bool paused_) external {
        paused = paused_;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        _transfer(from, to, amount);
        return true;
    }

    /// @notice Moves `value` from `from` to its caller, `to`, on `from`'s signature: once per
    /// nonce, after `validAfter` and before `validBefore`, as USDC's does.
    function receiveWithAuthorization(
        address from,
        address to,
        uint256 value,
        uint256 validAfter,
        uint256 validBefore,
        bytes32 nonce,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external {
        require(to == msg.sender, "caller must be the payee");
        require(block.timestamp > validAfter, "authorization is not yet valid");
        require(block.timestamp < validBefore, "authorization is expired");
        require(!authorizationState[from][nonce], "authorization is used");
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                DOMAIN_SEPARATOR(),
                keccak256(
                    abi.encode(
                        RECEIVE_WITH_AUTHORIZATION_TYPEHASH, from, to, value, validAfter, validBefore, nonce
                    )
                )
            )
        );
        require(uint256(s) <= HALF_N && (v == 27 || v == 28), "invalid signature");
        address signer = ecrecover(digest, v, r, s);
        require(signer != address(0) && signer == from, "invalid signature");
        authorizationState[from][nonce] = true;
        emit AuthorizationUsed(from, nonce);
        _transfer(from, to, value);
    }

    function DOMAIN_SEPARATOR() public view returns (bytes32) {
        return keccak256(
            abi.encode(
                DOMAIN_TYPEHASH,
                keccak256(bytes(name)),
                keccak256(bytes(version)),
                block.chainid,
                address(this)
            )
        );
    }

    function _transfer(address from, address to, uint256 amount) private {
        require(!paused, "paused");
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
}
