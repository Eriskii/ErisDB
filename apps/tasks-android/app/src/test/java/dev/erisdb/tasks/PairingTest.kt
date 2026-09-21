package dev.erisdb.tasks

import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Reading a pairing ticket, and deciding what one means when it arrives.
 *
 * A ticket is the whole config — where the core is and the capability to
 * speak with it — so every refusal here is a refusal to store half of one.
 */
class PairingTest {

    private val eid = "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06"
    private val other = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"
    private val tok = "erisdb1.eyJmYWNldHMiOltdfQ.sig"

    /** `erisdb://pair/<base64url-nopad(JSON)>`, the way a core cuts one. */
    private fun code(json: String): String =
        "erisdb://pair/" + java.util.Base64.getUrlEncoder().withoutPadding()
            .encodeToString(json.toByteArray(Charsets.UTF_8))

    private fun ticket(
        v: Any? = 1,
        name: String? = "my-laptop",
        eid: String? = this.eid,
        url: String? = null,
        token: String? = tok,
    ): String {
        val json = JSONObject()
        if (v != null) json.put("v", v)
        if (name != null) json.put("name", name)
        if (eid != null) json.put("eid", eid)
        if (url != null) json.put("url", url)
        if (token != null) json.put("token", token)
        return code(json.toString())
    }

    private fun ok(text: String): Ticket {
        val read = readTicket(text)
        assertTrue("expected a ticket, got $read", read is Pairing.Ok)
        return (read as Pairing.Ok).ticket
    }

    private fun refusal(text: String): String {
        val read = readTicket(text)
        assertTrue("expected a refusal, got $read", read is Pairing.Refused)
        return (read as Pairing.Refused).reason
    }

    // ------------------------------------------------------------ reading

    @Test
    fun aTicketCarriesWhereTheCoreIsAndWhatToSpeakWith() {
        val t = ok(ticket(url = "http://192.168.1.20:7700"))
        assertEquals("my-laptop", t.name)
        assertEquals(eid, t.eid)
        assertEquals("http://192.168.1.20:7700", t.url)
        assertEquals(tok, t.code)
    }

    @Test
    fun aNameIsOptionalBecauseNothingDependsOnIt() {
        assertNull(ok(ticket(name = null)).name)
    }

    @Test
    fun whitespaceAroundACodeIsIgnored() {
        assertEquals(eid, ok("  ${ticket()}\n").eid)
    }

    @Test
    fun anEndpointIdIsHeldInLowercaseHoweverItArrives() {
        assertEquals(eid, ok(ticket(eid = eid.uppercase())).eid)
    }

    // ------------------------------------------------------------ refusing

    @Test
    fun somethingThatIsNotATicketIsRefusedByShape() {
        for (text in listOf("", "hello", "https://example.com/pair/abc", "erisdb://paired/abc")) {
            assertTrue(refusal(text).contains("erisdb://pair/"))
        }
    }

    @Test
    fun aCodeThatDoesNotDecodeIsRefused() {
        assertTrue(refusal("erisdb://pair/not base64!").contains("base64url"))
        // Decodes cleanly, carries something that is not a ticket.
        assertTrue(refusal(code("not json at all")).contains("ticket"))
    }

    @Test
    fun anUnknownVersionIsRefusedRatherThanGuessedAt() {
        assertTrue(refusal(ticket(v = 2)).contains("version"))
        assertTrue(refusal(ticket(v = null)).contains("version"))
    }

    @Test
    fun aTicketWithNoTokenIsRefused() {
        assertTrue(refusal(ticket(token = null)).contains("token"))
        assertTrue(refusal(ticket(token = "")).contains("token"))
    }

    @Test
    fun aTicketNamingNoTransportIsRefused() {
        val reason = refusal(ticket(eid = null, url = null))
        assertTrue(reason.contains("endpoint id"))
        assertTrue(reason.contains("address"))
    }

    @Test
    fun anEndpointIdThatIsNot64HexIsRefused() {
        for (bad in listOf(eid.dropLast(1), eid + "0", eid.dropLast(1) + "g", "")) {
            assertTrue(refusal(ticket(eid = bad)).contains("64"))
        }
    }

    // -------------------------------------------------- what this app dials

    @Test
    fun aWebOnlyTicketIsRefusedBecauseThisAppSpeaksQuic() {
        val read = quicTicket(ticket(eid = null, url = "http://192.168.1.20:7700"))
        assertTrue(read is Pairing.Refused)
        val reason = (read as Pairing.Refused).reason
        assertTrue(reason.contains("http://192.168.1.20:7700"))
        assertTrue(reason.contains("endpoint id"))
    }

    @Test
    fun aTicketOfferingBothTransportsIsDialedOverIroh() {
        val read = quicTicket(ticket(url = "http://192.168.1.20:7700"))
        assertEquals(eid, ((read as Pairing.Ok)).ticket.eid)
    }

    // ------------------------------------------------------------ arriving

    @Test
    fun aTicketArrivingOnAnUnpairedPhonePairsIt() {
        val landed = arrival(ticket(), server = "", token = "")
        assertEquals(eid, (landed as Arrival.Pair).ticket.eid)
    }

    @Test
    fun aTicketArrivingOverAWorkingConfigAsksBeforeReplacingIt() {
        val landed = arrival(ticket(eid = other), server = eid, token = tok)
        assertTrue(landed is Arrival.Confirm)
        assertEquals(other, (landed as Arrival.Confirm).ticket.eid)
        assertFalse("a different core is a replacement, not a refresh", landed.sameCore)
    }

    @Test
    fun reScanningTheCoreAlreadyPairedIsARefreshOfItsToken() {
        val landed = arrival(ticket(token = "erisdb1.fresh.sig"), server = eid, token = tok)
        assertTrue((landed as Arrival.Confirm).sameCore)
    }

    @Test
    fun aPhoneHoldingACoreButNoTokenIsNotPairedAndTakesTheTicket() {
        // The keystore key is gone, so the token reads as absent. That is
        // an unpaired phone, and a ticket is exactly what it wants.
        assertTrue(arrival(ticket(), server = eid, token = "") is Arrival.Pair)
    }

    @Test
    fun aTicketThatCannotBeReadIsRefusedRatherThanRouted() {
        val landed = arrival("erisdb://pair/!!!", server = eid, token = tok)
        assertTrue((landed as Arrival.Refused).reason.contains("base64url"))
    }

    // ------------------------------------------------------ the front door

    @Test
    fun aConfiguredInstallOpensToItsTasksNotToPairing() {
        // Pairing is the front door for a phone that has never paired. One
        // that already holds a core and a token is not sent back through it.
        assertTrue(paired(eid, tok))
    }

    @Test
    fun anythingLessThanACoreAndATokenIsNotPaired() {
        assertFalse(paired("", tok))
        assertFalse(paired(eid, ""))
        assertFalse(paired("", ""))
    }

    @Test
    fun anAlreadyConfiguredInstallIsNotLoggedOut() = runTest {
        // The token on disk is one the core still honours: what changed is
        // how permissions are named, not the tokens. So an install that
        // was working resumes, and the grants arrive afterwards.
        assertEquals(Launch.Resume(eid, tok), launch(eid, tok))

        val core = FakeCore().apply { grants = listOf("tasks:*") }
        val held = (fetchGrants(core) as Read.Ok).value
        assertTrue(held.mayCreate)
        assertTrue(held.mayDelete)

        // No pairing code is redeemed on the way back in.
        assertEquals(emptyList<String>(), core.callsMatching("POST /v1/pair"))
    }

    @Test
    fun anInstallThatLostItsTokenGoesBackToTheFrontDoor() {
        assertEquals(Launch.Pair, launch(eid, ""))
        assertEquals(Launch.Pair, launch("", ""))
    }
}
