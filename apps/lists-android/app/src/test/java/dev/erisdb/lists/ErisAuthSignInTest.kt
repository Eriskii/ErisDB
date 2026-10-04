package dev.erisdb.lists

import dev.eris.auth.client.ErisAuth
import dev.erisdb.android.SignIn
import dev.erisdb.android.signInAnswer
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Signing in through ErisAuth on the phone: what this app asks it, and what its answers mean here. */
class ErisAuthSignInTest {
    @Test
    fun theAskNamesThisAppItsPermissionsAndItsKey() {
        val key = "ab".repeat(32)
        val ask = JSONObject(ErisAuth.askJson("erisdb", CLIENT, MANIFEST, ErisAuth.Identity.iroh(key)))
        assertEquals("pair", ask.getString("op"))
        assertEquals("erisdb", ask.getString("service"))
        assertEquals(CLIENT, ask.getString("client"))
        assertEquals(MANIFEST, List(MANIFEST.size) { ask.getJSONArray("requested").getString(it) })
        assertEquals("iroh", ask.getJSONObject("identity").getString("kind"))
        assertEquals(key, ask.getJSONObject("identity").getString("key"))
    }

    @Test
    fun answersAreApprovalsDenialsOrFailures() {
        assertEquals(ErisAuth.Answer.Approved(listOf("listsead")), ErisAuth.Answer.parse("""{"approved":["listsead"]}"""))
        assertEquals(ErisAuth.Answer.Denied("the person said no"), ErisAuth.Answer.parse("""{"denied":"the person said no"}"""))
        assertEquals(ErisAuth.Answer.Failed("not joined"), ErisAuth.Answer.parse("""{"error":"not joined"}"""))
        assertEquals(ErisAuth.Answer.Failed("ErisAuth sent something unreadable"), ErisAuth.Answer.parse("nonsense"))
    }

    @Test
    fun anApprovalLeavesCollectingTheCredentialToTheCore() {
        assertNull(signInAnswer(ErisAuth.Answer.Approved(listOf("listsead"))))
    }

    @Test
    fun aRefusalSaysWhyAndTheAppPairsByTicket() {
        assertEquals(
            SignIn.Declined("ErisAuth: the person said no"),
            signInAnswer(ErisAuth.Answer.Denied("the person said no")),
        )
        assertEquals(
            SignIn.Declined("ErisAuth: this device has not joined ErisDB"),
            signInAnswer(ErisAuth.Answer.Failed("this device has not joined ErisDB")),
        )
    }
}
