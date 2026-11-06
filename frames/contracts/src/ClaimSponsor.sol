// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.28;

import {HegotaSupport, Devnet, Scope, TxParam} from "@forshtat/hegota-support/contracts/HegotaSupport.sol";
import {ZecSwap} from "zecswap/ZecSwap.sol";

/// ERC-8421's staking registry, as the devnet's `NaiveStakingRegistry` implements it.
interface IStakingRegistry {
    function addStake(uint32 unstakeDelaySec) external payable;
}

/// @title ClaimSponsor
/// @notice Pays the gas of a forward swap's claim that the swap's own per-swap key sends as an
/// EIP-8141 frame transaction, so the key never holds ETH. Its pay frame approves the first
/// transaction of `terms.user` while the swap, one of `MAKER`'s paying an account, is open or
/// ready, at no more than `MAX_COST`.
/// @dev It checks neither the share, as Pallas costs more than a validation prefix may spend, nor
/// the window, as validation may not read the clock: a wrong or late share only makes the claim
/// frame revert. So each real swap costs it at most one transaction, which only the swap's key can
/// send. `MAKER` bounds how many swaps there are, as anyone may open swaps on ZecSwap. Reading
/// the swap reads another contract's storage, which ERC-8421's mempool allows only a staked payer
/// and EIP-8141's own mempool never. There is no owner: nobody can take its ETH or unlock its stake.
contract ClaimSponsor is HegotaSupport(Devnet.ERC8421_WORKSHOP) {
    /// The caller of every DEFAULT and VERIFY frame.
    address internal constant ENTRY_POINT = 0x00000000000000000000000000000000000000AA;

    ZecSwap public immutable SWAPS;
    address public immutable MAKER;
    IStakingRegistry public immutable REGISTRY;
    /// The most a sponsored transaction may cost: all its gas at its fee cap, in wei.
    uint256 public immutable MAX_COST;

    error NotEntryPoint();
    error WrongTerms();
    error NotClaimable();
    error NotSponsored();
    error WrongSender();
    error NotFirst();
    error TooCostly();

    /// Funded through its creation value or plain transfers.
    constructor(ZecSwap swaps, address maker, IStakingRegistry registry, uint256 maxCost) payable {
        SWAPS = swaps;
        MAKER = maker;
        REGISTRY = registry;
        MAX_COST = maxCost;
    }

    receive() external payable {}

    /// @notice Stakes what it is sent, for as long as `unstakeDelaySec`; nothing here can unlock it.
    function stake(uint32 unstakeDelaySec) external payable {
        REGISTRY.addStake{value: msg.value}(unstakeDelaySec);
    }

    /// @notice The pay frame of a claim of swap `id`, on `terms`.
    function sponsor(bytes32 id, ZecSwap.Terms calldata terms) external {
        if (msg.sender != ENTRY_POINT) revert NotEntryPoint();
        ZecSwap.Swap memory swap = SWAPS.getSwap(id);
        if (swap.termsHash != keccak256(abi.encode(terms))) revert WrongTerms();
        if (swap.stage != ZecSwap.Stage.Open && swap.stage != ZecSwap.Stage.Ready) revert NotClaimable();
        (bytes32 refundNote,) = SWAPS.reverseFunding(id);
        if (terms.maker != MAKER || terms.payoutNote != 0 || refundNote != 0) revert NotSponsored();
        if (address(uint160(txparam(TxParam.SENDER))) != terms.user) revert WrongSender();
        if (txparam(TxParam.NONCE) != 0) revert NotFirst();
        if (txparam(TxParam.MAX_COST) > MAX_COST) revert TooCostly();
        approve(Scope.PAYMENT);
    }
}
