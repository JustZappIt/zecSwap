package xyz.justzappit.atomicswap

/** libzecswap's entry points, implemented in `crates/zecswap-jni/src/lib.rs`. */
internal object AtomicSwapNative {
    init {
        System.loadLibrary("zecswap")
    }

    @JvmStatic external fun userShare(seed: ByteArray, mainnet: Boolean, index: Int): ByteArray

    @JvmStatic external fun authAddress(seed: ByteArray, mainnet: Boolean, index: Int): ByteArray

    @JvmStatic external fun claimSecret(seed: ByteArray, mainnet: Boolean, index: Int): ByteArray

    @JvmStatic external fun payoutNote(seed: ByteArray, mainnet: Boolean, index: Int): ByteArray

    @JvmStatic external fun accept(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        chainId: Long,
        contract: ByteArray,
        quoteId: ByteArray,
        makerShare: ByteArray,
        makerProof: ByteArray,
    ): ByteArray

    @JvmStatic external fun depositAccount(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        makerShare: ByteArray,
    ): Array<String>

    @JvmStatic external fun signLockClaim(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        chainId: Long,
        contract: ByteArray,
        swapId: ByteArray,
        deadline: Long,
    ): ByteArray

    @JvmStatic external fun signPayout(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        chainId: Long,
        contract: ByteArray,
        swapId: ByteArray,
        relayer: ByteArray,
        fee: String,
    ): ByteArray

    @JvmStatic external fun signRefund(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        makerShare: ByteArray,
        makerSecret: ByteArray,
        pczt: ByteArray,
    ): ByteArray

    @JvmStatic external fun railgunAddress(seed: ByteArray): String
    @JvmStatic external fun signReverseOpen(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        chainId: Long,
        contract: ByteArray,
        maker: ByteArray,
        token: ByteArray,
        amount: String,
        makerShare: ByteArray,
        readyDeadline: Long,
        refundAfter: Long,
        fundingDeadline: Long,
    ): ByteArray

    @JvmStatic external fun signReverseAction(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        chainId: Long,
        contract: ByteArray,
        swapId: ByteArray,
        deadline: Long,
        action: Int,
    ): ByteArray

    @JvmStatic external fun signRefundPayout(
        seed: ByteArray,
        mainnet: Boolean,
        index: Int,
        chainId: Long,
        contract: ByteArray,
        swapId: ByteArray,
        relayer: ByteArray,
        fee: String,
    ): ByteArray
}
