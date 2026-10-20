package dev.erisdb.tasks

import dev.eris.auth.client.ErisAuth
import dev.erisdb.android.*
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * Signing in through ErisAuth on the phone: what this app asks it, and what its answer comes to.
 * The answers are ErisAuth's reply lines as its client library reads them; the address is the
 * envelope erisdb-client's `nativeLocate` writes.
 */
class SignInTest {
    private val app = "ab".repeat(32)
    private val erisdb = "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06"
    private val server = """{"id":"$erisdb","addrs":[{"Relay":"https://relay.example/"},{"Ip":"192.0.2.7:4433"}]}"""
    private val note = """{"by":"9c1e","body":"eyJtZXNoIjoiNWYyZCJ9","sig":"Qm3t"}"""
    private val snapshot = """{"mesh":"5f2d","entries":[],"directory":[]}"""
    private val approved = ErisAuth.Answer.parse("""{"approved":{"note":$note,"snapshot":$snapshot}}""")

    /** `nativeLocate`, answering [envelope] for the snapshot ErisAuth handed over and nothing else. */
    private fun locate(envelope: String): (String) -> String = { seen ->
        assertEquals("the snapshot ErisAuth answered with", JSONObject(snapshot).toString(), JSONObject(seen).toString())
        envelope
    }

    private val nowhere: (String) -> String = { fail("nothing is located without an approval"); "" }

    @Test
    fun thisAppAsksToBeLetIntoErisDBAsAClientUnderItsOwnKey() {
        val line = JSONObject(Ask(CLIENT, MANIFEST, app).line)
        assertEquals("approve", line.getString("op"))
        assertEquals("erisdb", line.getString("service"))
        assertEquals(CLIENT, line.getString("name"))
        assertEquals(MANIFEST, List(MANIFEST.size) { line.getJSONArray("grants").getString(it) })
        assertEquals(false, line.getBoolean("serves"))
        assertEquals(app, line.getString("app"))
    }

    @Test
    fun anApprovalKeepsWhereErisDBIsBeforeItCountsAsSignedIn() {
        val kept = mutableListOf<String>()
        val signedIn = signedIn(approved, locate("""{"ok":true,"server":$server}""")) { kept += it }
        assertEquals(1, kept.size)
        assertEquals(JSONObject(server).toString(), JSONObject(kept.single()).toString())
        assertEquals(SignIn.SignedIn(kept.single()), signedIn)
        assertEquals(erisdb, serverId(kept.single()))
    }

    @Test
    fun anApprovalWhoseSnapshotNamesNoErisDBSignsNothingIn() {
        val kept = mutableListOf<String>()
        val signedIn = signedIn(
            approved,
            locate("""{"ok":false,"error":"this mesh has no ErisDB, or the snapshot does not say where it is"}"""),
        ) { kept += it }
        assertTrue(kept.isEmpty())
        assertTrue("$signedIn", signedIn is SignIn.Failed)
        assertTrue((signedIn as SignIn.Failed).why.contains("this mesh has no ErisDB"))
    }

    @Test
    fun anEnvelopeThatCannotBeReadSignsNothingIn() {
        for (envelope in listOf("", "not json", """{"ok":true}""")) {
            val kept = mutableListOf<String>()
            val signedIn = signedIn(approved, locate(envelope)) { kept += it }
            assertTrue(kept.isEmpty())
            assertTrue("$envelope: $signedIn", signedIn is SignIn.Failed)
        }
    }

    @Test
    fun aDenialIsThePersonsAndKeepsNothing() {
        val denied = ErisAuth.Answer.parse("""{"denied":"the person said no"}""")
        assertEquals(SignIn.Denied("the person said no"), signedIn(denied, nowhere) { fail("nothing is kept") })
    }

    @Test
    fun aFailureSaysWhyAndKeepsNothing() {
        val failed = ErisAuth.Answer.parse("""{"error":"that is not ErisDB's passphrase"}""")
        assertEquals(SignIn.Failed("that is not ErisDB's passphrase"), signedIn(failed, nowhere) { fail("nothing is kept") })
        val gone = ErisAuth.Answer.parse("""{"error":"ErisAuth went away"}""")
        assertEquals(SignIn.Failed("ErisAuth went away"), signedIn(gone, nowhere) { fail("nothing is kept") })
    }

    @Test
    fun aKeptServerIsKnownByItsEndpointId() {
        assertEquals(erisdb, serverId(server))
        assertEquals(erisdb, serverId(erisdb))
        assertEquals(erisdb, serverId(erisdb.uppercase()))
        assertEquals(erisdb, serverId("""{"id":"$erisdb","addrs":[]}"""))
        assertNull(serverId(""))
        assertNull(serverId("""{"addrs":[]}"""))
        assertNull(serverId("""{"id":"short","addrs":[]}"""))
        assertNull(serverId("not an address"))
    }
}
