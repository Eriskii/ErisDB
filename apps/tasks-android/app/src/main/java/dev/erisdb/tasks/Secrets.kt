package dev.erisdb.tasks

import android.annotation.SuppressLint
import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

// The capability token and the device's iroh private key are the two
// things on this phone worth stealing: one is write access to the store,
// the other is this device's identity on the network.
//
// They are sealed with an AES-256-GCM key the Android keystore generates
// and never hands out. What sits on disk is ciphertext, and the key that
// opens it cannot leave the device — so the bytes are useless anywhere
// the phone isn't.

/** A named place secrets are read from and written to. */
interface SecretStore {
    fun get(key: String): String?
    fun put(key: String, value: String)
    fun clear(key: String)
}

/**
 * Secrets sealed under a keystore key, one file of `base64(iv‖ciphertext)`.
 *
 * A value that will not open reads as absent. That is the honest answer:
 * the keystore key is gone — wiped with the lock screen on some devices,
 * or never restored because this app's data is not backed up — and no
 * amount of retrying will bring the plaintext back. The user pastes a
 * fresh token; the identity is minted again.
 */
@SuppressLint("ApplySharedPref")
class KeystoreSecrets(ctx: Context) : SecretStore {
    private val prefs = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    override fun get(key: String): String? {
        val sealed = prefs.getString(key, null) ?: return null
        return try {
            val raw = Base64.decode(sealed, Base64.NO_WRAP)
            val cipher = Cipher.getInstance(TRANSFORM)
            cipher.init(
                Cipher.DECRYPT_MODE,
                key(),
                GCMParameterSpec(TAG_BITS, raw, 0, IV_BYTES),
            )
            String(cipher.doFinal(raw, IV_BYTES, raw.size - IV_BYTES), Charsets.UTF_8)
        } catch (_: Exception) {
            null
        }
    }

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
        (store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
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
        const val ALIAS = "erisdb.tasks.secrets"
        const val TRANSFORM = "AES/GCM/NoPadding"
        const val IV_BYTES = 12
        const val TAG_BITS = 128
    }
}
