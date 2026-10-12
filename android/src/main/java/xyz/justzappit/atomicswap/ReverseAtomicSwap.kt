package xyz.justzappit.atomicswap

import java.math.BigInteger

/** Reverse swaps reuse AtomicSwap's acceptance, joint account, and Railgun note derivation. */
object ReverseAtomicSwap {
    /** Commits the refund to its note in [railgun]'s wallet. */
    fun signOpen(
        key: SwapKey,
        railgun: RailgunSeed,
        deployment: Deployment,
        terms: ReverseEscrowTerms,
    ): ByteArray =
        AtomicSwapNative.signReverseOpen(
            key.seed,
            key.mainnet,
            key.index,
            railgun.bytes,
            deployment.chainId,
            deployment.contract,
            terms.maker,
            terms.token,
            terms.amount.toString(),
            terms.makerShare,
            terms.readyDeadline,
            terms.refundAfter,
            terms.fundingDeadline,
        )

    /** Authorize only after independently scanning the joint account and confirming the full ZEC deposit. */
    fun signReady(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        deadline: Long,
    ): ByteArray = signAction(key, deployment, swapId, deadline, READY)

    fun signLockRefund(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        deadline: Long,
    ): ByteArray = signAction(key, deployment, swapId, deadline, LOCK_REFUND)

    fun signRefundPayout(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        relayer: ByteArray,
        fee: BigInteger,
    ): ByteArray =
        AtomicSwapNative.signRefundPayout(
            key.seed,
            key.mainnet,
            key.index,
            deployment.chainId,
            deployment.contract,
            swapId,
            relayer,
            fee.toString(),
        )

    fun signRefundRescue(
        key: SwapKey,
        railgun: RailgunSeed,
        deployment: Deployment,
        swapId: ByteArray,
        relayer: ByteArray,
        fee: BigInteger,
        nonce: Long,
        deadline: Long,
    ): ByteArray =
        AtomicSwapNative.signRefundRescue(
            key.seed,
            key.mainnet,
            key.index,
            railgun.bytes,
            deployment.chainId,
            deployment.contract,
            swapId,
            relayer,
            fee.toString(),
            nonce,
            deadline,
        )

    /** Use the maker's share from a verified Claimed escrow to sweep the received ZEC home. */
    fun signReceive(
        key: SwapKey,
        makerShare: ByteArray,
        makerSecret: ByteArray,
        pczt: ByteArray,
        intent: SweepIntent,
    ): ByteArray = AtomicSwap.signRefund(key, makerShare, makerSecret, pczt, intent)

    private fun signAction(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        deadline: Long,
        action: Int,
    ): ByteArray =
        AtomicSwapNative.signReverseAction(
            key.seed,
            key.mainnet,
            key.index,
            deployment.chainId,
            deployment.contract,
            swapId,
            deadline,
            action,
        )

    private const val READY = 0
    private const val LOCK_REFUND = 1
}

class ReverseEscrowTerms(
    val maker: ByteArray,
    val token: ByteArray,
    val amount: BigInteger,
    val makerShare: ByteArray,
    val readyDeadline: Long,
    val refundAfter: Long,
    val fundingDeadline: Long,
) {
    init {
        require(maker.size == 20 && token.size == 20)
        require(makerShare.size == 64)
        require(amount.signum() > 0 && amount.bitLength() <= 128)
        require(fundingDeadline > 0 && fundingDeadline < readyDeadline && readyDeadline < refundAfter)
    }
}
