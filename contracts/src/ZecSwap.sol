// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Pallas} from "./Pallas.sol";

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @title ZecSwap
/// @notice Settles atomic swaps of shielded ZEC for ERC-20 tokens held by makers.
/// @dev The ZEC sits in an Orchard address whose spend key is ±(e + z): the maker holds e,
/// the user holds z, and this contract stores E = [e]·G and Z = [z]·G. Revealing z credits
/// the user (and lets the maker sweep the ZEC); revealing e refunds the maker (and lets the
/// user sweep it back). A reveal is only accepted under a lock its party took in an earlier
/// transaction, and the other party cannot lock while it is held, so a reveal can never
/// lose a race and leave both halves public. A lock that lapses unused hands the other party
/// the next turn, so neither can lock the other out for good. Reveals make no token calls either: payouts are
/// withdrawn separately, so a paused or blacklisting token cannot revert a reveal.
contract ZecSwap {
    enum Stage {
        None,
        Open,
        Ready,
        Claimed,
        Refunded
    }

    struct Swap {
        address maker;
        uint64 t0;
        Stage stage;
        address user;
        uint64 t1;
        address token;
        uint64 claimLockUntil;
        uint128 amount;
        uint64 refundLockUntil;
        uint256 makerX;
        uint256 makerY;
        uint256 userX;
        uint256 userY;
        /// The share revealed at settlement: the user's once Claimed, the maker's once Refunded.
        /// Kept in storage so the other party can read it without querying logs.
        uint256 secret;
    }

    /// @notice How long a lock gives its holder to land the reveal.
    uint256 public immutable LOCK_DURATION;

    mapping(bytes32 id => Swap) private swaps;
    mapping(bytes32 makerKey => bool) public makerKeyUsed;
    /// @notice What each account can withdraw: makers' inventory and users' claimed payouts.
    mapping(address owner => mapping(address token => uint256)) public balanceOf;

    event Deposited(address indexed owner, address indexed token, uint256 amount);
    event Withdrawn(address indexed owner, address indexed token, uint256 amount, address to);
    event Opened(
        bytes32 indexed id,
        address indexed maker,
        address indexed user,
        address token,
        uint256 amount,
        uint256[2] makerKey,
        uint256[2] userKey,
        uint64 t0,
        uint64 t1
    );
    event MarkedReady(bytes32 indexed id);
    event ClaimLocked(bytes32 indexed id, uint64 until);
    event Claimed(bytes32 indexed id, uint256 userSecret);
    event RefundLocked(bytes32 indexed id, uint64 until);
    event Refunded(bytes32 indexed id, uint256 makerSecret);

    error ZeroAmount();
    error ZeroAddress();
    error InsufficientBalance();
    error InvalidKey();
    error KeyReused();
    error InvalidDeadlines();
    error Unauthorized();
    error WrongStage();
    error LockUnavailable();
    error LockNotHeld();
    error WrongSecret();
    error TransferFailed();

    constructor(uint256 lockDuration) {
        if (lockDuration == 0 || lockDuration > type(uint32).max) revert InvalidDeadlines();
        LOCK_DURATION = lockDuration;
    }

    /// @notice Adds to the caller's inventory, from which it opens swaps.
    function deposit(address token, uint256 amount) external {
        if (amount == 0) revert ZeroAmount();
        _call(token, abi.encodeCall(IERC20.transferFrom, (msg.sender, address(this), amount)));
        balanceOf[msg.sender][token] += amount;
        emit Deposited(msg.sender, token, amount);
    }

    function withdraw(address token, uint256 amount, address to) external {
        if (to == address(0)) revert ZeroAddress();
        uint256 balance = balanceOf[msg.sender][token];
        if (amount > balance) revert InsufficientBalance();
        balanceOf[msg.sender][token] = balance - amount;
        _call(token, abi.encodeCall(IERC20.transfer, (to, amount)));
        emit Withdrawn(msg.sender, token, amount, to);
    }

    /// @notice Commits `amount` of the caller's inventory to a swap with the holder of `userKey`.
    /// @param t0 After this the user may claim without the maker's `ready`.
    /// @param t1 After this the maker may refund a `Ready` swap.
    function open(
        address token,
        uint128 amount,
        uint256[2] calldata makerKey,
        uint256[2] calldata userKey,
        address user,
        uint64 t0,
        uint64 t1
    ) external returns (bytes32 id) {
        if (amount == 0) revert ZeroAmount();
        if (user == address(0)) revert ZeroAddress();
        if (t0 <= block.timestamp || t1 <= t0) revert InvalidDeadlines();
        if (
            !Pallas.isOnCurve(makerKey[0], makerKey[1]) || !Pallas.isOnCurve(userKey[0], userKey[1])
                || makerKey[0] == userKey[0]
        ) revert InvalidKey();

        id = swapId(userKey);
        bytes32 makerKeyHash = keccak256(abi.encode(makerKey));
        if (swaps[id].stage != Stage.None || makerKeyUsed[makerKeyHash]) revert KeyReused();
        uint256 balance = balanceOf[msg.sender][token];
        if (amount > balance) revert InsufficientBalance();

        makerKeyUsed[makerKeyHash] = true;
        balanceOf[msg.sender][token] = balance - amount;
        swaps[id] = Swap({
            maker: msg.sender,
            t0: t0,
            stage: Stage.Open,
            user: user,
            t1: t1,
            token: token,
            claimLockUntil: 0,
            amount: amount,
            refundLockUntil: 0,
            makerX: makerKey[0],
            makerY: makerKey[1],
            userX: userKey[0],
            userY: userKey[1],
            secret: 0
        });
        emit Opened(id, msg.sender, user, token, amount, makerKey, userKey, t0, t1);
    }

    /// @notice The maker attests that the ZEC deposit is confirmed, giving up its right to
    /// cancel before `t1`.
    function ready(bytes32 id) external {
        Swap storage swap = swaps[id];
        if (msg.sender != swap.maker) revert Unauthorized();
        if (swap.stage != Stage.Open || swap.refundLockUntil != 0) revert WrongStage();
        swap.stage = Stage.Ready;
        emit MarkedReady(id);
    }

    /// @notice Reserves the claim for `LOCK_DURATION`, at any time once claimable: a claim never
    /// expires. It waits out a held refund lock, and the maker's turn after a claim lock of the
    /// user's lapsed unused.
    function lockClaim(bytes32 id) external {
        Swap storage swap = swaps[id];
        if (msg.sender != swap.user) revert Unauthorized();
        Stage stage = swap.stage;
        bool claimable = stage == Stage.Ready || (stage == Stage.Open && block.timestamp >= swap.t0);
        if (!claimable) revert WrongStage();
        if (_locked(swap) || _turnAfterLapse(swap.claimLockUntil, swap.refundLockUntil)) {
            revert LockUnavailable();
        }
        uint64 until = uint64(block.timestamp + LOCK_DURATION);
        swap.claimLockUntil = until;
        emit ClaimLocked(id, until);
    }

    /// @notice Reveals the user's share and credits the amount to the user, to withdraw at
    /// will. Callable by anyone while the claim lock is held.
    function claim(bytes32 id, uint256 userSecret) external {
        Swap storage swap = swaps[id];
        if (swap.stage != Stage.Open && swap.stage != Stage.Ready) revert WrongStage();
        if (block.timestamp >= swap.claimLockUntil) revert LockNotHeld();
        if (!Pallas.isSpendAuthMul(userSecret, swap.userX, swap.userY)) revert WrongSecret();
        swap.stage = Stage.Claimed;
        swap.secret = userSecret;
        balanceOf[swap.user][swap.token] += swap.amount;
        emit Claimed(id, userSecret);
    }

    /// @notice Reserves the refund for `LOCK_DURATION`: any time before `ready`, or once `t1` has
    /// passed. It waits out a held claim lock, and the user's turn after a refund lock of the
    /// maker's lapsed unused.
    function lockRefund(bytes32 id) external {
        Swap storage swap = swaps[id];
        if (msg.sender != swap.maker) revert Unauthorized();
        Stage stage = swap.stage;
        bool refundable = stage == Stage.Open || (stage == Stage.Ready && block.timestamp >= swap.t1);
        if (!refundable) revert WrongStage();
        if (_locked(swap) || _turnAfterLapse(swap.refundLockUntil, swap.claimLockUntil)) {
            revert LockUnavailable();
        }
        uint64 until = uint64(block.timestamp + LOCK_DURATION);
        swap.refundLockUntil = until;
        emit RefundLocked(id, until);
    }

    /// @notice Reveals the maker's share and returns the amount to its inventory. Callable by
    /// anyone while the refund lock is held.
    function refund(bytes32 id, uint256 makerSecret) external {
        Swap storage swap = swaps[id];
        if (swap.stage != Stage.Open && swap.stage != Stage.Ready) revert WrongStage();
        if (block.timestamp >= swap.refundLockUntil) revert LockNotHeld();
        if (!Pallas.isSpendAuthMul(makerSecret, swap.makerX, swap.makerY)) revert WrongSecret();
        swap.stage = Stage.Refunded;
        swap.secret = makerSecret;
        balanceOf[swap.maker][swap.token] += swap.amount;
        emit Refunded(id, makerSecret);
    }

    function getSwap(bytes32 id) external view returns (Swap memory) {
        return swaps[id];
    }

    /// @notice A swap is keyed by the user's public share, so a user can find its swap from
    /// its own key and can never have one share in two swaps.
    function swapId(uint256[2] calldata userKey) public pure returns (bytes32) {
        return keccak256(abi.encode(userKey));
    }

    function _locked(Swap storage swap) private view returns (bool) {
        return block.timestamp < swap.claimLockUntil || block.timestamp < swap.refundLockUntil;
    }

    /// @dev Whether the other side still has the turn that follows `own`, a lock of the caller's
    /// that lapsed unused after `other` last ran. Turns alternate, so neither side can keep the
    /// other out: whoever is online when its turn comes can settle.
    function _turnAfterLapse(uint64 own, uint64 other) private view returns (bool) {
        return own > other && block.timestamp < uint256(own) + LOCK_DURATION;
    }

    function _call(address token, bytes memory data) private {
        (bool ok, bytes memory result) = token.call(data);
        if (!ok || (result.length == 0 ? token.code.length == 0 : !abi.decode(result, (bool)))) {
            revert TransferFailed();
        }
    }
}
