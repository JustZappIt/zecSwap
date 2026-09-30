package xyz.justzappit.atomicswap

import java.math.BigInteger

/**
 * The user's cryptography for swaps that sell shielded ZEC into their Railgun balance, over
 * libzecswap. Stateless: each call derives the swap's keys from [SwapKey] again, so nothing secret
 * stays here. Failures throw [AtomicSwapException].
 */
object AtomicSwap {
    /** `Z`, the public share a quote is accepted with (64 bytes, `x ‖ y`). */
    fun userShare(key: SwapKey): ByteArray = AtomicSwapNative.userShare(key.seed, key.mainnet, key.index)

    /** The swap's `user` on the contract: its own key, which signs for it and is never funded. */
    fun authAddress(key: SwapKey): ByteArray = AtomicSwapNative.authAddress(key.seed, key.mainnet, key.index)

    /** `z`, the secret a claim reveals. */
    fun claimSecret(key: SwapKey): ByteArray = AtomicSwapNative.claimSecret(key.seed, key.mainnet, key.index)

    /** The note the payout is shielded to, in [railgun]'s wallet. */
    fun payoutNote(
        key: SwapKey,
        railgun: RailgunSeed,
    ): PayoutNote {
        val words = AtomicSwapNative.payoutNote(key.seed, key.mainnet, key.index, railgun.bytes).asWords()
        return PayoutNote(npk = words[0], encryptedBundle = words.subList(1, 4), shieldKey = words[4], commitment = words[5])
    }

    /**
     * Checks the maker's proof of its share for this quote, then proves ours, bound to both shares
     * and the payout into [railgun]'s wallet.
     */
    fun accept(
        key: SwapKey,
        railgun: RailgunSeed,
        deployment: Deployment,
        quoteId: ByteArray,
        makerShare: ByteArray,
        makerProof: ByteArray,
    ): Acceptance {
        val bytes =
            AtomicSwapNative.accept(
                key.seed,
                key.mainnet,
                key.index,
                railgun.bytes,
                deployment.chainId,
                deployment.contract,
                quoteId,
                makerShare,
                makerProof,
            )
        return Acceptance(
            userShare = bytes.copyOfRange(0, SHARE_BYTES),
            userProof = bytes.copyOfRange(SHARE_BYTES, 2 * SHARE_BYTES),
            viewingKeys = bytes.copyOfRange(2 * SHARE_BYTES, 3 * SHARE_BYTES),
        )
    }

    /** From the maker share as the contract records it, never as a quote reports it. */
    fun depositAccount(
        key: SwapKey,
        makerShare: ByteArray,
    ): DepositAccount {
        val (address, ufvk) = AtomicSwapNative.depositAccount(key.seed, key.mainnet, key.index, makerShare)
        return DepositAccount(address, ufvk)
    }

    /** `r ‖ s ‖ v` over the EIP-712 `LockClaim(swapId, deadline)`, for a relayer to send. */
    fun signLockClaim(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        deadline: Long,
    ): ByteArray =
        AtomicSwapNative.signLockClaim(
            key.seed,
            key.mainnet,
            key.index,
            deployment.chainId,
            deployment.contract,
            swapId,
            deadline,
        )

    /** `r ‖ s ‖ v` over the EIP-712 `Payout(swapId, relayer, fee)`: what the relayer may keep. */
    fun signPayout(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        relayer: ByteArray,
        fee: BigInteger,
    ): ByteArray =
        AtomicSwapNative.signPayout(
            key.seed,
            key.mainnet,
            key.index,
            deployment.chainId,
            deployment.contract,
            swapId,
            relayer,
            fee.toString(),
        )

    /**
     * Signs the PCZT that sweeps a refunded deposit home, with the maker's secret the contract
     * revealed. Refuses a secret that doesn't match the share, or a PCZT that spends anything else.
     */
    fun signRefund(
        key: SwapKey,
        makerShare: ByteArray,
        makerSecret: ByteArray,
        pczt: ByteArray,
    ): ByteArray = AtomicSwapNative.signRefund(key.seed, key.mainnet, key.index, makerShare, makerSecret, pczt)

    /** The `0zk` address of [railgun]'s wallet, the one Railgun's own apps open from its words. */
    fun railgunAddress(railgun: RailgunSeed): String = AtomicSwapNative.railgunAddress(railgun.bytes)

    private const val SHARE_BYTES = 64
    private const val WORD_BYTES = 32

    private fun ByteArray.asWords() = (indices step WORD_BYTES).map { copyOfRange(it, it + WORD_BYTES) }
}

/** Swap [index] of the wallet whose 64-byte BIP-39 seed is [seed]. */
class SwapKey(
    val seed: ByteArray,
    val mainnet: Boolean,
    val index: Int,
)

/** The 64-byte BIP-39 seed of the Railgun wallet's mnemonic: the wallet payouts and refunds go to. */
class RailgunSeed(
    val bytes: ByteArray,
)

/** A ZecSwap contract: the EIP-712 domain the swap's signatures are bound to. */
class Deployment(
    val chainId: Long,
    val contract: ByteArray,
)

/** The Railgun note a payout is shielded to. The quote names its [commitment]; the relayer sends the rest. */
class PayoutNote(
    val npk: ByteArray,
    val encryptedBundle: List<ByteArray>,
    val shieldKey: ByteArray,
    val commitment: ByteArray,
)

/** What accepting a quote sends the maker. */
class Acceptance(
    val userShare: ByteArray,
    val userProof: ByteArray,
    val viewingKeys: ByteArray,
)

/** Where the ZEC goes, and the viewing key that watches it for a refund. */
data class DepositAccount(
    val address: String,
    val ufvk: String,
)

class AtomicSwapException(
    message: String,
) : Exception(message)
