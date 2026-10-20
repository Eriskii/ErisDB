package dev.erisdb.tasks

import dev.erisdb.android.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The app's key and the ErisDB it was let into, kept together in the sealed store. The store
 * here keeps its values in memory; on the phone they are sealed under the Android keystore.
 */
class CredentialsTest {
    private class Memory : SecretStore {
        val values = LinkedHashMap<String, String>()
        override fun get(key: String): String? = values[key]
        override fun put(key: String, value: String) {
            values[key] = value
        }
        override fun clear(key: String) {
            values.remove(key)
        }
    }

    private val erisdb = "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06"
    private val other = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"
    private fun at(id: String, ip: String) = """{"id":"$id","addrs":[{"Ip":"$ip:4433"}]}"""

    @Test
    fun theKeyIsMadeOnceAndKept() {
        val memory = Memory()
        val key = Credentials(memory).key()
        assertTrue(key, key.length == 64 && key.all { it in "0123456789abcdef" })
        assertEquals(key, Credentials(memory).key())
        assertNotEquals(key, Credentials(Memory()).key())
    }

    @Test
    fun anAppWithAKeyButNoServerIsNotSignedIn() {
        val credentials = Credentials(Memory())
        credentials.key()
        assertNull(credentials.server)
    }

    @Test
    fun theServerIsKeptBesideTheKeyAndOutlastsTheLaunch() {
        val memory = Memory()
        val first = Credentials(memory)
        val key = first.key()
        first.keep(at(erisdb, "192.0.2.7"))

        val next = Credentials(memory)
        assertEquals(key, next.key())
        assertEquals(at(erisdb, "192.0.2.7"), next.server)
        assertEquals(setOf(key, at(erisdb, "192.0.2.7")), memory.values.values.toSet())
    }

    @Test
    fun anotherErisDBIsAMoveAndTheSameOneElsewhereIsNot() {
        val credentials = Credentials(Memory())
        credentials.key()
        assertTrue("the first ErisDB is a move from none", credentials.keep(at(erisdb, "192.0.2.7")))
        assertFalse("the same ErisDB at new addresses", credentials.keep(at(erisdb, "198.51.100.2")))
        assertEquals(at(erisdb, "198.51.100.2"), credentials.server)
        assertTrue("another ErisDB", credentials.keep(at(other, "198.51.100.2")))
    }

    @Test
    fun aKeyMadeAfreshHasBeenLetInNowhere() {
        // The sealed store lost the key (the keystore key went with the lock screen) but not the
        // address: the address was for the old key, which no new key can use.
        val memory = Memory()
        val old = Credentials(memory)
        val key = old.key()
        old.keep(at(erisdb, "192.0.2.7"))
        memory.clear(memory.values.entries.first { it.value == key }.key)

        val next = Credentials(memory)
        assertNull(next.server)
        assertNotEquals(key, next.key())
        assertNull(next.server)
        assertFalse(memory.values.containsValue(at(erisdb, "192.0.2.7")))
    }
}
