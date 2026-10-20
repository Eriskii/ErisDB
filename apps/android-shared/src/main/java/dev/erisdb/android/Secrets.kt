package dev.erisdb.android

import android.annotation.SuppressLint
import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

// The app's Iroh key is its credential: ErisDB lets in the key a device
// approved, and nothing else. It is sealed, with the address of the ErisDB
// it was let into, under an AES-256-GCM key the Android keystore generates
// and never hands out. What sits on disk is ciphertext, and the key that
// opens it cannot leave the device — so the bytes are useless anywhere the
// phone isn't.

/** A named place secrets are read from and written to. */
interface SecretStore {
    fun get(key: String): String?
    fun put(key: String, value: String)
    fun clear(key: String)
}

/**
 * Secrets sealed under the keystore key [alias], one preference of `base64(iv‖ciphertext)` each.
 *
 * A value that will not open reads as absent. That is the honest answer: the keystore key is
 * gone — wiped with the lock screen on some devices, or never restored because this app's data
 * is not backed up — and no amount of retrying will bring the plaintext back. The app makes a new
 * key and signs in again.
 */
@SuppressLint("ApplySharedPref")
class KeystoreSecrets(ctx: Context, private val alias: String) : SecretStore {
    private val prefs = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    override fun get(key: String): String? {
        val sealed = prefs.getString(key, null) ?: return null
        return try {
            val raw = Base64.decode(sealed, Base64.NO_WRAP)
            val cipher = Cipher.getInstance(TRANSFORM)
            cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(TAG_BITS, raw, 0, IV_BYTES))
            String(cipher.doFinal(raw, IV_BYTES, raw.size - IV_BYTES), Charsets.UTF_8)
        } catch (_: Exception) {
            null
        }
    }

    /** Durable before returning: commit(), not apply(). */
    override fun put(key: String, value: String) {
        val cipher = Cipher.getInstance(TRANSFORM).apply { init(Cipher.ENCRYPT_MODE, key()) }
        val sealed = cipher.iv + cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        prefs.edit().putString(key, Base64.encodeToString(sealed, Base64.NO_WRAP)).commit()
    }

    override fun clear(key: String) {
        prefs.edit().remove(key).commit()
    }

    private fun key(): SecretKey {
        val store = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (store.getEntry(alias, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return generator.generateKey()
    }

    private companion object {
        const val FILE = "erisdb-secrets"
        const val KEYSTORE = "AndroidKeyStore"
        const val TRANSFORM = "AES/GCM/NoPadding"
        const val IV_BYTES = 12
        const val TAG_BITS = 128
    }
}

/**
 * The app's key and the ErisDB it was let into, kept together in [secrets].
 *
 * The key is made once and kept for good: it is what a device approved. The address goes with
 * the key it was let in under, so a key made afresh starts with none.
 */
class Credentials(private val secrets: SecretStore) {
    /** The app's Iroh key: 32 random bytes, as 64 hex characters. */
    fun key(): String {
        secrets.get(KEY)?.let { return it }
        val hex = ByteArray(32).also { SecureRandom().nextBytes(it) }.joinToString("") { "%02x".format(it) }
        secrets.clear(SERVER)
        secrets.put(KEY, hex)
        return hex
    }

    /** Where the ErisDB this key was let into is, as `nativeLocate` gave it; null before it was. */
    val server: String?
        get() = if (secrets.get(KEY) == null) null else secrets.get(SERVER)

    /** Keep [server] beside the key, durably. True when it is another ErisDB than the one kept. */
    fun keep(server: String): Boolean {
        val moved = this.server?.let(::serverId) != serverId(server)
        secrets.put(SERVER, server)
        return moved
    }

    private companion object {
        const val KEY = "key"
        const val SERVER = "server"
    }
}
