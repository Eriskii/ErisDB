package dev.erisdb.lists

import dev.erisdb.android.*

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** What a token is worth, and when the core has stopped honouring it. */
class CapabilityTest {

    private val now = 1_800_000_000L

    @Test
    fun aTokenWithNoExpiryIsNeverRefreshed() {
        assertEquals(Lifetime.Permanent, lifetimeOf(null, now))
    }

    @Test
    fun aLifetimeIsWhatIsLeftOfIt() {
        assertEquals(Lifetime.Seconds(86_400), lifetimeOf(now + 86_400, now))
    }

    @Test
    fun anExpiryThatHasArrivedIsADeadTokenNotAShortOne() {
        // `erisdb mint --ttl 0` mints exp == now. Recording that as a
        // one-second lifetime leaves the app refreshing a token the core
        // already rejects, forever; it is dead and it says so.
        assertEquals(Lifetime.Dead, lifetimeOf(now, now))
        assertEquals(Lifetime.Dead, lifetimeOf(now - 1, now))
    }

    @Test
    fun aNumericStatusSettlesARefreshRejection() {
        assertTrue(refreshRejected(JSONObject().put("ok", false).put("status", 401)))
        assertFalse(refreshRejected(JSONObject().put("ok", false).put("status", 503)))
    }

    @Test
    fun withoutOneTheErrorTextIsRead() {
        assertTrue(refreshRejected(JSONObject().put("error", "core said 401 Unauthorized")))
        assertTrue(refreshRejected(JSONObject().put("error", "capability expired")))
        assertFalse(refreshRejected(JSONObject().put("error", "connection refused")))
    }
}
