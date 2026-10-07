// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Pallas} from "./Pallas.sol";
import {IRailgun, ShieldVault} from "./ShieldVault.sol";
import {Token, IERC20} from "./Token.sol";

/// @title ZecSwap
/// @notice Settles atomic swaps of shielded ZEC for ERC-20 tokens held by makers.
/// @dev The ZEC sits in an Orchard address whose spend key is ±(e + z): the maker holds e,
/// the user holds z, and this contract stores E = [e]·G and Z = [z]·G. Revealing z pays the user
/// (and lets the maker sweep the ZEC); revealing e refunds the maker (and lets the user sweep it
/// back). A reveal is only accepted under a lock its party took in an earlier transaction, and the
/// other party cannot lock while it is held, so a reveal can never lose a race and leave both
/// halves public. A lock that lapses unused hands the other party the next turn, so neither can
/// lock the other out for good. Reveals make no token calls either: payouts leave separately, so a
/// paused or blacklisting token cannot revert a reveal.
///
/// A claim pays either the user's account, which withdraws it, or, where Railgun is deployed, the
/// user's Railgun balance: the maker commits at open to a note the user built, and `payout` can
/// only shield to it. Such a user has no account on this chain. Its `user` is a key of that swap
/// alone, which signs for the claim lock and the payout, and relayers send them.
///
/// A swap's terms never change after open, so only their hash is stored: every call on a swap
/// supplies them again, and the call reverts unless they hash to what open stored. `Opened`
/// emits them for the parties to keep.
contract ZecSwap {
    enum Stage {
        None,
        Open,
        Ready,
        Claimed,
        Refunded
    }

    struct Terms {
        address maker;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        address user;
        /// After this the user may claim without the maker's `ready`.
        uint64 t0;
        /// After this the maker may refund a `Ready` swap.
        uint64 t1;
        /// The `noteCommitment` of the Railgun note a claim pays; zero pays `user`'s balance.
        bytes32 payoutNote;
    }

    struct Swap {
        /// `hashTerms` of the terms the swap opened with.
        bytes32 termsHash;
        Stage stage;
        /// Whether a Railgun payout or reverse refund has left.
        bool paidOut;
        uint64 claimLockUntil;
        uint64 refundLockUntil;
        /// The share revealed at settlement: the user's once Claimed, the maker's once Refunded.
        /// Kept in storage so the other party can read it without querying logs.
        uint256 secret;
    }

    struct ReverseOpen {
        address maker;
        address user;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        uint64 t0;
        uint64 t1;
        bytes32 refundNote;
        uint64 deadline;
    }

    struct ReverseFunding {
        bytes32 refundNote;
        uint64 blockNumber;
    }

    mapping(bytes32 id => ReverseFunding) public reverseFunding;
    mapping(bytes32 id => uint64) public rescueNonces;
    bytes32 private constant OPEN_REVERSE_TYPEHASH = keccak256(
        "OpenReverse(address maker,address user,address token,uint128 amount,bytes32 makerKey,bytes32 userKey,uint64 t0,uint64 t1,bytes32 refundNote,uint64 deadline)"
    );
    bytes32 private constant READY_TYPEHASH = keccak256("Ready(bytes32 id,uint64 deadline)");
    bytes32 private constant LOCK_REFUND_TYPEHASH = keccak256("LockRefund(bytes32 id,uint64 deadline)");
    bytes32 private constant REFUND_PAYOUT_TYPEHASH =
        keccak256("RefundPayout(bytes32 id,address relayer,uint128 fee)");

    bytes32 private constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    bytes32 private constant LOCK_CLAIM_TYPEHASH = keccak256("LockClaim(bytes32 id,uint64 deadline)");
    bytes32 private constant PAYOUT_TYPEHASH = keccak256("Payout(bytes32 id,address relayer,uint128 fee)");
    bytes32 private constant RESCUE_TYPEHASH =
        keccak256("Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)");
    /// secp256k1's n / 2. A signature with a larger `s` is the malleated twin of one without.
    uint256 private constant HALF_N = 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;

    /// @notice How long a lock gives its holder to land the reveal.
    uint256 public immutable LOCK_DURATION;
    /// @notice Railgun's shield entry point; zero where claims can only pay accounts.
    IRailgun public immutable RAILGUN;
    /// @notice The code every swap's vault runs: each is an EIP-1167 proxy to this one.
    ShieldVault public immutable VAULT_LOGIC;
    bytes32 private immutable VAULT_CODE_HASH;

    mapping(bytes32 id => Swap) private swaps;
    /// @notice Keyed by `keccak256(abi.encode(maker, makerKey))`: a maker's share is single-use.
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
        uint64 t1,
        bytes32 payoutNote
    );
    event MarkedReady(bytes32 indexed id);
    event ClaimLocked(bytes32 indexed id, uint64 until);
    event Claimed(bytes32 indexed id, uint256 userSecret);
    event PaidOut(bytes32 indexed id, address relayer, uint256 fee);
    event Rescued(bytes32 indexed id, address relayer, uint256 fee);
    event RefundLocked(bytes32 indexed id, uint64 until);
    event Refunded(bytes32 indexed id, uint256 makerSecret);

    error ZeroAmount();
    error InvalidAmount();
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
    error NoShieldedPayouts();
    error WrongNote();
    error BadSignature();
    error Expired();
    error VaultNotDeployed();
    error WrongTerms();

    constructor(uint256 lockDuration, IRailgun railgun) {
        if (lockDuration == 0 || lockDuration > type(uint32).max) revert InvalidDeadlines();
        LOCK_DURATION = lockDuration;
        RAILGUN = railgun;
        VAULT_LOGIC = new ShieldVault();
        VAULT_CODE_HASH = keccak256(_vaultCode());
    }

    /// @notice Adds to the caller's inventory, from which it opens swaps.
    function deposit(address token, uint256 amount) external {
        if (amount == 0) revert ZeroAmount();
        Token.transferFrom(token, msg.sender, address(this), amount);
        balanceOf[msg.sender][token] += amount;
        emit Deposited(msg.sender, token, amount);
    }

    function withdraw(address token, uint256 amount, address to) external {
        if (to == address(0)) revert ZeroAddress();
        uint256 balance = balanceOf[msg.sender][token];
        if (amount > balance) revert InsufficientBalance();
        balanceOf[msg.sender][token] = balance - amount;
        Token.transfer(token, to, amount);
        emit Withdrawn(msg.sender, token, amount, to);
    }

    /// @notice Commits `amount` of the caller's inventory to a swap with the holder of `userKey`,
    /// on the terms `Terms` describes, the caller as their `maker`.
    function open(
        address token,
        uint128 amount,
        uint256[2] calldata makerKey,
        uint256[2] calldata userKey,
        address user,
        uint64 t0,
        uint64 t1,
        bytes32 payoutNote
    ) external returns (bytes32 id) {
        return _open(Terms(msg.sender, token, amount, makerKey, userKey, user, t0, t1, payoutNote));
    }

    function _open(Terms memory terms) private returns (bytes32 id) {
        if (terms.amount == 0) revert ZeroAmount();
        if (terms.user == address(0)) revert ZeroAddress();
        if (terms.payoutNote != 0 && address(RAILGUN) == address(0)) revert NoShieldedPayouts();
        if (terms.t0 <= block.timestamp || terms.t1 <= terms.t0) revert InvalidDeadlines();
        uint256[2] memory makerKey = terms.makerKey;
        uint256[2] memory userKey = terms.userKey;
        if (
            !Pallas.isOnCurve(makerKey[0], makerKey[1]) || !Pallas.isOnCurve(userKey[0], userKey[1])
                || makerKey[0] == userKey[0]
        ) revert InvalidKey();

        id = swapId(terms.maker, userKey);
        _record(id, terms);
        emit Opened(
            id,
            terms.maker,
            terms.user,
            terms.token,
            terms.amount,
            makerKey,
            userKey,
            terms.t0,
            terms.t1,
            terms.payoutNote
        );
    }

    /// @dev Debits the inventory open locks, spends the maker's share and stores the swap.
    function _record(bytes32 id, Terms memory terms) private {
        bytes32 makerKeyHash = keccak256(abi.encode(terms.maker, terms.makerKey));
        Swap storage swap = swaps[id];
        if (swap.stage != Stage.None || makerKeyUsed[makerKeyHash]) revert KeyReused();
        uint256 balance = balanceOf[terms.maker][terms.token];
        if (terms.amount > balance) revert InsufficientBalance();

        makerKeyUsed[makerKeyHash] = true;
        balanceOf[terms.maker][terms.token] = balance - terms.amount;
        // `hashTerms`, from memory: a static struct encodes the same wherever it is held, and
        // hashing calldata in place keeps every later call's check cheaper.
        swap.termsHash = keccak256(abi.encode(terms));
        swap.stage = Stage.Open;
    }

    /// @notice Called by Relay Adapt after unshielding and approving the exact escrow amount.
    /// The stored roles remain USDC side (`maker`) and ZEC side (`user`) in both directions.
    function openReverse(ReverseOpen calldata terms, bytes calldata signature)
        external
        returns (bytes32 id)
    {
        if (block.timestamp > terms.deadline) revert Expired();
        if (terms.user == address(0) || terms.maker == address(0)) revert ZeroAddress();
        if (terms.refundNote == 0) revert WrongNote();
        if (terms.amount > type(uint120).max) revert InvalidAmount();
        if (terms.deadline >= terms.t0) revert InvalidDeadlines();
        if (address(RAILGUN) == address(0)) revert NoShieldedPayouts();
        _checkSignature(
            terms.user,
            keccak256(
                abi.encode(
                    OPEN_REVERSE_TYPEHASH,
                    terms.maker,
                    terms.user,
                    terms.token,
                    terms.amount,
                    keccak256(abi.encode(terms.makerKey)),
                    keccak256(abi.encode(terms.userKey)),
                    terms.t0,
                    terms.t1,
                    terms.refundNote,
                    terms.deadline
                )
            ),
            signature
        );
        // Record before the token call. A failed or short transfer rolls the entire open back.
        balanceOf[terms.user][terms.token] += terms.amount;
        id = _open(
            Terms(
                terms.user,
                terms.token,
                terms.amount,
                terms.userKey,
                terms.makerKey,
                terms.maker,
                terms.t0,
                terms.t1,
                0
            )
        );
        reverseFunding[id] = ReverseFunding(terms.refundNote, uint64(block.number));
        uint256 beforeBalance = IERC20(terms.token).balanceOf(address(this));
        Token.transferFrom(terms.token, msg.sender, address(this), terms.amount);
        if (IERC20(terms.token).balanceOf(address(this)) != beforeBalance + terms.amount) {
            revert InsufficientBalance();
        }
    }

    function readyWithSig(bytes32 id, Terms calldata terms, uint64 deadline, bytes calldata signature)
        external
    {
        Swap storage swap = _load(id, terms);
        if (block.timestamp > deadline) revert Expired();
        if (reverseFunding[id].refundNote == 0) revert WrongStage();
        _checkSignature(terms.maker, keccak256(abi.encode(READY_TYPEHASH, id, deadline)), signature);
        _ready(id, swap, terms);
    }

    function lockRefundWithSig(bytes32 id, Terms calldata terms, uint64 deadline, bytes calldata signature)
        external
    {
        Swap storage swap = _load(id, terms);
        if (block.timestamp > deadline) revert Expired();
        if (deadline >= block.timestamp + LOCK_DURATION) revert InvalidDeadlines();
        if (reverseFunding[id].refundNote == 0) revert WrongStage();
        _checkSignature(terms.maker, keccak256(abi.encode(LOCK_REFUND_TYPEHASH, id, deadline)), signature);
        _lockRefund(id, swap, terms);
    }

    function refundPayout(
        bytes32 id,
        Terms calldata terms,
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint128 fee,
        bytes calldata signature
    ) external {
        Swap storage swap = _load(id, terms);
        if (swap.stage != Stage.Refunded || swap.paidOut) revert WrongStage();
        bytes32 note = reverseFunding[id].refundNote;
        if (note == 0 || noteCommitment(npk, ciphertext) != note) revert WrongNote();
        _checkSignature(
            terms.maker, keccak256(abi.encode(REFUND_PAYOUT_TYPEHASH, id, msg.sender, fee)), signature
        );
        _shieldPayout(id, swap, terms, npk, ciphertext, fee);
    }

    /// @notice The maker attests that the ZEC deposit is confirmed, giving up its right to
    /// cancel before `t1`.
    function ready(bytes32 id, Terms calldata terms) external {
        Swap storage swap = _load(id, terms);
        if (msg.sender != terms.maker) revert Unauthorized();
        _ready(id, swap, terms);
    }

    function _ready(bytes32 id, Swap storage swap, Terms calldata terms) private {
        if (reverseFunding[id].refundNote != 0 && block.timestamp >= terms.t0) revert Expired();
        if (swap.stage != Stage.Open || swap.refundLockUntil != 0) revert WrongStage();
        swap.stage = Stage.Ready;
        emit MarkedReady(id);
    }

    /// @notice Reserves the claim for `LOCK_DURATION`, at any time once claimable: a claim never
    /// expires. It waits out a held refund lock, and the maker's turn after a claim lock of the
    /// user's lapsed unused.
    function lockClaim(bytes32 id, Terms calldata terms) external {
        Swap storage swap = _load(id, terms);
        if (msg.sender != terms.user) revert Unauthorized();
        _lockClaim(id, swap, terms);
    }

    /// @notice `lockClaim` on the signature of the swap's `user`, sent by anyone. The lock it
    /// takes outlasts `deadline`, so one signature takes at most one lock.
    function lockClaimWithSig(bytes32 id, Terms calldata terms, uint64 deadline, bytes calldata signature)
        external
    {
        Swap storage swap = _load(id, terms);
        if (block.timestamp > deadline) revert Expired();
        if (deadline >= block.timestamp + LOCK_DURATION) revert InvalidDeadlines();
        _checkSignature(terms.user, keccak256(abi.encode(LOCK_CLAIM_TYPEHASH, id, deadline)), signature);
        _lockClaim(id, swap, terms);
    }

    /// @notice Reveals the user's share and credits the amount to the user to withdraw at will,
    /// or holds it for `payout` into Railgun. Callable by anyone while the claim lock is held.
    function claim(bytes32 id, Terms calldata terms, uint256 userSecret) external {
        Swap storage swap = _load(id, terms);
        if (swap.stage != Stage.Open && swap.stage != Stage.Ready) revert WrongStage();
        if (block.timestamp >= swap.claimLockUntil) revert LockNotHeld();
        if (!Pallas.isSpendAuthMul(userSecret, terms.userKey[0], terms.userKey[1])) revert WrongSecret();
        swap.stage = Stage.Claimed;
        swap.secret = userSecret;
        if (terms.payoutNote == 0) balanceOf[terms.user][terms.token] += terms.amount;
        emit Claimed(id, userSecret);
    }

    /// @notice Shields a claimed swap's amount into Railgun, to the note committed at open, less
    /// the fee its user signed for the relayer that sends this. A step apart from `claim`, so
    /// Railgun or the token failing can delay the payout but never the reveal.
    function payout(
        bytes32 id,
        Terms calldata terms,
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint128 fee,
        bytes calldata signature
    ) external {
        Swap storage swap = _load(id, terms);
        if (swap.stage != Stage.Claimed || terms.payoutNote == 0 || swap.paidOut) revert WrongStage();
        if (noteCommitment(npk, ciphertext) != terms.payoutNote) revert WrongNote();
        _checkSignature(terms.user, keccak256(abi.encode(PAYOUT_TYPEHASH, id, msg.sender, fee)), signature);
        _shieldPayout(id, swap, terms, npk, ciphertext, fee);
    }

    function _shieldPayout(
        bytes32 id,
        Swap storage swap,
        Terms calldata terms,
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint128 fee
    ) private {
        swap.paidOut = true;
        bytes memory code = _vaultCode();
        address vault;
        assembly ("memory-safe") {
            vault := create2(0, add(code, 0x20), mload(code), id)
        }
        if (vault == address(0)) revert VaultNotDeployed();
        Token.transfer(terms.token, vault, terms.amount);
        ShieldVault(vault).shield(RAILGUN, terms.token, npk, ciphertext, fee, msg.sender);
        emit PaidOut(id, msg.sender, fee);
    }

    /// @notice Shields whatever came back to a paid-out swap's vault again, to a note its user
    /// signs for, less the fee it signs for the relayer that sends this.
    function rescue(
        bytes32 id,
        Terms calldata terms,
        bytes32 npk,
        IRailgun.ShieldCiphertext calldata ciphertext,
        uint128 fee,
        uint64 nonce,
        uint64 deadline,
        bytes calldata signature
    ) external {
        if (!_load(id, terms).paidOut) revert WrongStage();
        if (block.timestamp > deadline) revert Expired();
        if (nonce != rescueNonces[id]) revert BadSignature();
        bytes32 note = noteCommitment(npk, ciphertext);
        _checkSignature(
            reverseFunding[id].refundNote == 0 ? terms.user : terms.maker,
            keccak256(abi.encode(RESCUE_TYPEHASH, id, note, msg.sender, fee, nonce, deadline)),
            signature
        );
        rescueNonces[id] = nonce + 1;
        ShieldVault(vaultOf(id)).shield(RAILGUN, terms.token, npk, ciphertext, fee, msg.sender);
        emit Rescued(id, msg.sender, fee);
    }

    /// @notice Reserves the refund for `LOCK_DURATION`: any time before `ready`, or once `t1` has
    /// passed. It waits out a held claim lock, and the user's turn after a refund lock of the
    /// maker's lapsed unused.
    function lockRefund(bytes32 id, Terms calldata terms) external {
        Swap storage swap = _load(id, terms);
        if (msg.sender != terms.maker) revert Unauthorized();
        _lockRefund(id, swap, terms);
    }

    function _lockRefund(bytes32 id, Swap storage swap, Terms calldata terms) private {
        Stage stage = swap.stage;
        bool refundable = stage == Stage.Open || (stage == Stage.Ready && block.timestamp >= terms.t1);
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
    function refund(bytes32 id, Terms calldata terms, uint256 makerSecret) external {
        Swap storage swap = _load(id, terms);
        if (swap.stage != Stage.Open && swap.stage != Stage.Ready) revert WrongStage();
        if (block.timestamp >= swap.refundLockUntil) revert LockNotHeld();
        if (!Pallas.isSpendAuthMul(makerSecret, terms.makerKey[0], terms.makerKey[1])) revert WrongSecret();
        swap.stage = Stage.Refunded;
        swap.secret = makerSecret;
        if (reverseFunding[id].refundNote == 0) balanceOf[terms.maker][terms.token] += terms.amount;
        emit Refunded(id, makerSecret);
    }

    /// @notice A swap's state. Its terms are only stored hashed; stage `None` means no swap.
    function getSwap(bytes32 id) external view returns (Swap memory) {
        return swaps[id];
    }

    /// @notice What a swap stores of the terms it opened with, and checks every later call's
    /// against: their ABI encoding, hashed.
    function hashTerms(Terms calldata terms) public pure returns (bytes32) {
        return keccak256(abi.encode(terms));
    }

    /// @notice A swap is keyed by its maker and the user's public share, so a user finds its swap
    /// from its own key, a share is never in two of one maker's swaps, and nobody can take a
    /// swap's id by opening under someone else's share.
    function swapId(address maker, uint256[2] memory userKey) public pure returns (bytes32) {
        return keccak256(abi.encode(maker, userKey));
    }

    /// @notice What a swap commits to as its payout note.
    function noteCommitment(bytes32 npk, IRailgun.ShieldCiphertext calldata ciphertext)
        public
        pure
        returns (bytes32)
    {
        return keccak256(abi.encode(npk, ciphertext));
    }

    /// @notice Where a swap's Railgun payout leaves from, and where Railgun sends it back.
    function vaultOf(bytes32 id) public view returns (address) {
        return address(
            uint160(uint256(keccak256(abi.encodePacked(bytes1(0xff), address(this), id, VAULT_CODE_HASH))))
        );
    }

    /// @dev EIP-1167 creation code of a minimal proxy to `VAULT_LOGIC`: a vault costs a fraction of
    /// deploying the logic itself.
    function _vaultCode() private view returns (bytes memory) {
        return abi.encodePacked(
            hex"3d602d80600a3d3981f3363d3d373d3d3d363d73",
            address(VAULT_LOGIC),
            hex"5af43d82803e903d91602b57fd5bf3"
        );
    }

    /// @dev The swap `id` names, if `terms` are those it opened with. Every function that acts on
    /// a swap reads its terms through here, from its caller; a swap that doesn't exist has none.
    function _load(bytes32 id, Terms calldata terms) private view returns (Swap storage swap) {
        swap = swaps[id];
        if (swap.termsHash != hashTerms(terms)) revert WrongTerms();
    }

    function _lockClaim(bytes32 id, Swap storage swap, Terms calldata terms) private {
        Stage stage = swap.stage;
        bool claimable = stage == Stage.Ready
            || (reverseFunding[id].refundNote == 0 && stage == Stage.Open && block.timestamp >= terms.t0);
        if (!claimable) revert WrongStage();
        if (_locked(swap) || _turnAfterLapse(swap.claimLockUntil, swap.refundLockUntil)) {
            revert LockUnavailable();
        }
        uint64 until = uint64(block.timestamp + LOCK_DURATION);
        swap.claimLockUntil = until;
        emit ClaimLocked(id, until);
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

    /// @dev Checks an EIP-712 signature by `signer` over `structHash`, under this contract's
    /// domain on this chain.
    function _checkSignature(address signer, bytes32 structHash, bytes calldata signature) private view {
        bytes32 domain = keccak256(
            abi.encode(DOMAIN_TYPEHASH, keccak256("ZecSwap"), keccak256("1"), block.chainid, address(this))
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", domain, structHash));
        if (signature.length != 65 || uint256(bytes32(signature[32:64])) > HALF_N) revert BadSignature();
        address recovered =
            ecrecover(digest, uint8(signature[64]), bytes32(signature[:32]), bytes32(signature[32:64]));
        if (recovered == address(0) || recovered != signer) revert BadSignature();
    }
}
