package xyz.justzappit.atomicswap

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.BeforeClass
import org.junit.Test
import java.io.File
import java.math.BigInteger
import java.security.KeyPairGenerator
import java.security.interfaces.RSAPrivateCrtKey
import java.util.Base64

/** Each token binding through the JNI layer, against an issuer key made here; RFC 9578's math is tested in Rust. */
class PrivacyPassTest {
    @Test
    fun aTokenRoundTripsThroughTheBindings() {
        val keys = KeyPairGenerator.getInstance("RSA").apply { initialize(2048) }.generateKeyPair()
        val issuer = keys.private as RSAPrivateCrtKey
        // RFC 9578's RSASSA-PSS SubjectPublicKeyInfo around the modulus, as its test vectors encode a 2048-bit key.
        val spki = SPKI_HEAD.bytes() + issuer.modulus.fixed() + "0203010001".bytes()
        // The redemption context is 32 bytes ending in the UTC day, here day 20 000.
        val challenge = "0002000b6973737565722e74657374" + "20" + "00".repeat(24) + "0000000000004e20" + "00056d616b6572"
        val header = "PrivateToken challenge=\"${challenge.bytes().base64Url()}\", token-key=\"${spki.base64Url()}\""

        val asked = PrivacyPass.readChallenge(header)
        assertEquals("issuer.test", asked.issuer)
        assertTrue(asked.challenge.contentEquals(challenge.bytes()))
        assertTrue(asked.tokenKey.contentEquals(spki))
        val request = PrivacyPass.blind(asked.challenge, asked.tokenKey)
        val signature = BigInteger(1, request.blinded).modPow(issuer.privateExponent, issuer.modulus)
        val token = PrivacyPass.finalize(request.pending, asked.tokenKey, signature.fixed())

        assertEquals(354, token.size)
        assertEquals("PrivateToken token=\"${token.base64Url()}\"", PrivacyPass.authorization(token))
        assertEquals("PendingToken", request.pending.toString())
        assertThrows(AtomicSwapException::class.java) {
            PrivacyPass.finalize(request.pending, asked.tokenKey, ByteArray(256) { 1 })
        }
    }

    private fun String.bytes() = chunked(2).map { it.toInt(16).toByte() }.toByteArray()

    private fun BigInteger.fixed() = toByteArray().let { ByteArray(maxOf(0, 256 - it.size)) + it.takeLast(256) }

    private fun ByteArray.base64Url() = Base64.getUrlEncoder().withoutPadding().encodeToString(this)

    companion object {
        private const val SPKI_HEAD =
            "30820152303d06092a864886f70d01010a3030a00d300b0609608648016503040202a11a30180609" +
                "2a864886f70d010108300b0609608648016503040202a2030201300382010f003082010a0282010100"

        @BeforeClass
        @JvmStatic
        fun hostLibrary() {
            val found =
                System.getProperty("java.library.path").orEmpty().split(File.pathSeparator).any {
                    File(it, System.mapLibraryName("zecswap")).exists()
                }
            assumeTrue("build libzecswap for the host first: cargo build -p zecswap-jni", found)
        }
    }
}
