package xyz.justzappit.atomicswap

/**
 * Privacy Pass tokens (RFC 9578, type `0x0002`) for the maker's accepts, over libzecswap. Stateless: a request's
 * client half travels as a [PendingToken] between [blind] and [finalize]. Failures throw [AtomicSwapException].
 */
object PrivacyPass {
    /** What a `WWW-Authenticate: PrivateToken` header asks for. */
    fun readChallenge(header: String): PrivateTokenChallenge {
        val (challenge, tokenKey, issuer) = AtomicSwapNative.readTokenChallenge(header)
        return PrivateTokenChallenge(challenge, issuer.decodeToString(), tokenKey)
    }

    /** A fresh token request for [challenge] under [tokenKey] (SPKI). */
    fun blind(
        challenge: ByteArray,
        tokenKey: ByteArray,
    ): PrivateTokenRequest {
        val bytes = AtomicSwapNative.blindToken(tokenKey, challenge)
        val request =
            PrivateTokenRequest(
                blinded = bytes.copyOfRange(0, BLINDED_BYTES),
                pending = PendingToken(bytes.copyOfRange(BLINDED_BYTES, bytes.size)),
            )
        bytes.fill(0)
        return request
    }

    /** The 354-byte token [pending] becomes once [blindSignature] unblinds to a valid signature under [tokenKey]. */
    fun finalize(
        pending: PendingToken,
        tokenKey: ByteArray,
        blindSignature: ByteArray,
    ): ByteArray = AtomicSwapNative.finalizeToken(pending.bytes, tokenKey, blindSignature)

    /** The `Authorization` value that spends [token]. */
    fun authorization(token: ByteArray): String = AtomicSwapNative.tokenAuthorization(token)

    private const val BLINDED_BYTES = 256
}

/** A token challenge's encoding, the issuer it names, and the key (SPKI) it asks tokens under. */
class PrivateTokenChallenge(
    val challenge: ByteArray,
    val issuer: String,
    val tokenKey: ByteArray,
)

/** What the issuer signs, and the client's half that finalizes its answer. */
class PrivateTokenRequest(
    val blinded: ByteArray,
    val pending: PendingToken,
)

/** A request's client half: whoever holds it can tie the token to the request, so it stays on the device. */
class PendingToken(
    val bytes: ByteArray,
) {
    override fun toString() = "PendingToken"
}
