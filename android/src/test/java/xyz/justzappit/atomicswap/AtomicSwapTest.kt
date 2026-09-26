package xyz.justzappit.atomicswap

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.BeforeClass
import org.junit.Test
import java.io.File
import java.math.BigInteger

/**
 * Through the JNI layer, the known answers `cargo run -p zecswap-client --example vectors` prints,
 * on a host build of libzecswap (`cargo build -p zecswap-jni`). Skipped without one.
 */
class AtomicSwapTest {
    private val key = SwapKey(ByteArray(64) { 7 }, mainnet = false, index = 0)
    private val deployment = Deployment(11_155_111, ByteArray(20) { 0x11 })
    private val swapId = "0x297f1ca9d44ff7136dbddb0720ecadc040229e22c03ad0f9e4c648212bfc7b66".bytes()
    private val makerShare =
        (
            "0x187d300ebb59a5c9e7c9e61debd0b535a8a5cbbad4a5c46ac2a0597a06c13ee4" +
                "329a369cf900745cdca87863f3f00de64d16e088bb1f5da341a2977547f9080a"
        ).bytes()

    @Test
    fun keys() {
        assertEquals(
            "0x0b42629d5b3f787aba7ccde87574c13f6db6b8fccb7c5cfeb3f4e4081c756461" +
                "311662156525fcaa692aeef5d2362c2a9ab2083cd6d968c618aec72617aa925a",
            AtomicSwap.userShare(key).hex(),
        )
        assertEquals(
            "0x01bfb1d4b12d5365d9bc7aa9e7e7f641bdb7cce5c647b29c7077f344111ba6ac",
            AtomicSwap.claimSecret(key).hex(),
        )
        assertEquals("0x757de38c2d9880e44ab59827d1622403fbf88ff5", AtomicSwap.authAddress(key).hex())
        assertEquals(
            "0zk1qyt5x0c632363rrmd8psxws6n9tscm8gps277gzc0w3s5cg4mdpe9rv7j6fe3z53luahk4ksjwagt68fl2" +
                "vguye054rxjyqzvhs9usq4rwrk09al6n0677pdrgn",
            AtomicSwap.railgunAddress(key.seed),
        )
    }

    @Test
    fun payoutNote() {
        val note = AtomicSwap.payoutNote(key)
        assertEquals("0x11eb0b931cc092fe6876f395a4f8d29cf5c97c43903605253386e008ab4881e3", note.npk.hex())
        assertEquals(
            listOf(
                "0xe9f4508256863b2559259a39333a65685dfd7a7c86fa101e959517e06aa798c6",
                "0x63835c785858b4cf0519698cc24ee8d84fb3f943d073895b3e2521517419ee97",
                "0x82981c9149e1977d0ca708cd313450306e87b35a41dc832ec419686055b02e9a",
            ),
            note.encryptedBundle.map { it.hex() },
        )
        assertEquals("0x02356776cb176876b31960b8ccbf0c0850a76e9a2ef49caa6631bca732d064b2", note.shieldKey.hex())
        assertEquals("0x14d061e0bf2b24d75b75adb91c04cc6b82be6b407b5901dbdb86bbbabe7a9acd", note.commitment.hex())
    }

    @Test
    fun depositAccount() {
        val account = AtomicSwap.depositAccount(key, makerShare)
        assertEquals(
            "utest1exuj2qh9gcll0zjygvk7c48e5ra40wwtvdgd2u0ygwn2g3kanp47utpgh25m4pqwcjqsqy55zyr7qncw0ct0gpeccytje6" +
                "tgeyaheah7",
            account.address,
        )
        assertTrue(account.ufvk.startsWith("uviewtest130mkztp6gdvnvn0k3y3h820wjn9unqj2lzkt400lgw4velrg6dsy8m4lp3jn"))
    }

    @Test
    fun signatures() {
        assertEquals(
            "0xc2a0a598fc3027f949c2a3f3fadb3e988a74effa69e09efc86d3db0e89904dfc" +
                "69a81a0cc62da15628ed8084bb31d05723dfc2da4308dfbeb0ef4e134b7497551b",
            AtomicSwap.signLockClaim(key, deployment, swapId, 1_790_000_000).hex(),
        )
        assertEquals(
            "0x883118048774977816784fb0f34f256a8344caaf134201e0acd9f0b5c1f8e6a3" +
                "685df4aa58f4d1725f49fbaa106cc594ef0ffe4b8569550860a00ce5291bae5b1c",
            AtomicSwap.signPayout(key, deployment, swapId, ByteArray(20) { 0x22 }, BigInteger.valueOf(20_000)).hex(),
        )
    }

    @Test
    fun failuresBecomeExceptions() {
        assertThrows(AtomicSwapException::class.java) { AtomicSwap.userShare(SwapKey(ByteArray(32), false, 0)) }
        val refused =
            assertThrows(AtomicSwapException::class.java) {
                AtomicSwap.accept(key, deployment, ByteArray(32), makerShare, ByteArray(64))
            }
        assertTrue(refused.message.orEmpty().startsWith("the maker's share proof"))
        assertThrows(AtomicSwapException::class.java) {
            AtomicSwap.signRefund(key, makerShare, ByteArray(32) { 1 }, ByteArray(0))
        }
    }

    private fun ByteArray.hex() = "0x" + joinToString("") { "%02x".format(it) }

    private fun String.bytes() = removePrefix("0x").chunked(2).map { it.toInt(16).toByte() }.toByteArray()

    companion object {
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
