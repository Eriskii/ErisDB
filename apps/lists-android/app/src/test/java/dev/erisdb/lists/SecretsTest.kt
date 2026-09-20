package dev.erisdb.lists

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Carrying the token and the iroh private key out of the clear. */
class SecretsTest {

    private class Memory(vararg pairs: Pair<String, String>) : SecretStore {
        val values = linkedMapOf(*pairs)
        override fun get(key: String): String? = values[key]
        override fun put(key: String, value: String) { values[key] = value }
        override fun clear(key: String) { values.remove(key) }
    }

    private val keys = listOf("token", "identity")

    @Test
    fun anExistingInstallKeepsItsTokenAndItsIdentity() {
        val plain = Memory("token" to "bz1.abc", "identity" to "deadbeef", "server" to "node1")
        val sealed = Memory()

        migrateSecrets(plain, sealed, keys)

        assertEquals("bz1.abc", sealed.get("token"))
        assertEquals("deadbeef", sealed.get("identity"))
        // Nothing worth stealing is left where it was.
        assertNull(plain.get("token"))
        assertNull(plain.get("identity"))
        // Anything that is not a secret stays put.
        assertEquals("node1", plain.get("server"))
    }

    @Test
    fun aSecondRunCarriesNothingAndBreaksNothing() {
        val plain = Memory()
        val sealed = Memory("token" to "bz1.abc")

        migrateSecrets(plain, sealed, keys)

        assertEquals("bz1.abc", sealed.get("token"))
    }

    @Test
    fun aTokenRefreshedSinceTheMigrationIsNotRolledBack() {
        // The plaintext copy is stale by definition: it is whatever was
        // there before the app started sealing its secrets.
        val plain = Memory("token" to "bz1.stale")
        val sealed = Memory("token" to "bz1.fresh")

        migrateSecrets(plain, sealed, keys)

        assertEquals("bz1.fresh", sealed.get("token"))
        assertNull(plain.get("token"))
    }
}
