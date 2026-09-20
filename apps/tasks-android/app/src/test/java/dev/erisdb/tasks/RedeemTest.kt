package dev.erisdb.tasks

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Two-phase pairing: redeem a code, then wait for a person.
 *
 * The token comes back exactly once — collecting it clears it from the
 * session — so the tests that matter most here are about what happens to
 * it in the instant after it arrives.
 */
class RedeemTest {

    private fun core() = FakeCore()

    // ------------------------------------------------------------ redeem

    @Test
    fun redeemingSaysWhoTheAppIsAndWhatItWants() = runTest {
        val core = core()
        assertEquals(Approval.Waiting, requestPairing(core, CLIENT, MANIFEST))

        val asked = core.redeemed.single()
        assertEquals(CLIENT, asked.getString("client"))
        val wanted = (0 until asked.getJSONArray("requested").length())
            .map { asked.getJSONArray("requested").getString(it) }
        assertEquals(MANIFEST, wanted)
        assertEquals("requested", core.pairStatus)
    }

    @Test
    fun aTicketCarryingACapabilityTokenIsNotAPairingCode() = runTest {
        // The old shape: a QR that handed over authority directly. The
        // code has to hold meta:pairing:redeem and nothing else, so a
        // token that does not is refused rather than adopted.
        val core = core().apply { mayRedeem = false }
        val out = requestPairing(core, CLIENT, MANIFEST)
        assertTrue(out is Approval.Over)
        assertTrue((out as Approval.Over).reason.contains("pairing code"))
    }

    @Test
    fun aCodeThatHasRunOutOfMinutesSaysSoAndStops() = runTest {
        val core = core().apply { pairExpired = true }
        val out = requestPairing(core, CLIENT, MANIFEST)
        assertTrue(out is Approval.Over)
        assertTrue((out as Approval.Over).reason.contains("expired"))
    }

    @Test
    fun anUnreachableCoreIsWorthAnotherTryRatherThanAnEnding() = runTest {
        val core = core().apply { offline = true }
        assertTrue(requestPairing(core, CLIENT, MANIFEST) is Approval.Unreachable)
    }

    @Test
    fun aCodeThisAppAlreadyRedeemedIsWaitedOnRatherThanRefused() = runTest {
        // The app was killed between redeeming and collecting. The session
        // knows where it stands, so the second run goes and asks it.
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        assertEquals(Approval.Waiting, requestPairing(core, CLIENT, MANIFEST))
    }

    // ------------------------------------------------------------ collect

    @Test
    fun aRedeemedCodeWaitsUntilAPersonAnswers() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        assertEquals(Approval.Waiting, collect(core) { fail("nothing to keep yet") })

        core.approve(listOf("tasks:read", "tasks:create"), "bz1.granted.sig")
        val out = collect(core) {}
        assertEquals("bz1.granted.sig", (out as Approval.Approved).token)
        assertEquals(listOf("tasks:read", "tasks:create"), out.granted)
    }

    @Test
    fun theCollectedTokenIsOnDiskBeforeCollectReturns() = runTest {
        // It is handed over exactly once. Losing it after collection costs
        // the user another trip to the core, so it is written through the
        // sealed store before anything else in this app can fail.
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.approve(listOf("tasks:*"), "bz1.only.chance")

        var kept: String? = null
        val out = collect(core) { kept = it }

        assertEquals("bz1.only.chance", kept)
        assertEquals("bz1.only.chance", (out as Approval.Approved).token)
        // And the core no longer holds it: a second ask cannot rescue one
        // that was dropped.
        assertNull(core.pairToken)
    }

    @Test
    fun aTokenAlreadyCollectedCannotBeFetchedAgain() = runTest {
        // The core spends the session in the write that hands the token
        // over, so a replayed code sees `collected` and nothing else.
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.approve(listOf("tasks:*"), "bz1.spent")
        collect(core) {}
        assertEquals("collected", core.pairStatus)

        val out = collect(core) { fail("there is nothing left to keep") }
        assertTrue(out is Approval.Over)
        assertTrue((out as Approval.Over).reason.contains("already"))
    }

    @Test
    fun aDeniedRequestEndsTheConversation() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.deny()
        assertEquals(Approval.Denied, collect(core) { fail("denied grants nothing") })
    }

    @Test
    fun aCodeThatExpiredWhileWaitingSaysSo() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.pairExpired = true
        val out = collect(core) { fail("an expired code grants nothing") }
        assertTrue(out is Approval.Over)
        assertTrue((out as Approval.Over).reason.contains("expired"))
    }

    @Test
    fun aPollThatNeverArrivesIsWorthPollingAgain() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.offline = true
        val out = collect(core) { fail("nothing arrived") }
        assertTrue(out is Approval.Unreachable)
        // Unreachable and Waiting are the two states the poll loop keeps
        // going through; the rest are endings.
        assertTrue(pollingOn(out))
        assertTrue(pollingOn(Approval.Waiting))
        assertFalse(pollingOn(Approval.Denied))
        assertFalse(pollingOn(Approval.Over("gone")))
        assertFalse(pollingOn(Approval.Approved("bz1.x", emptyList())))
    }

    @Test
    fun anApprovalNarrowerThanTheRequestIsCarriedNotComplainedAbout() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.approve(listOf("tasks:read"), "bz1.narrow")

        val out = collect(core) {} as Approval.Approved
        val held = Grants(out.granted)
        assertTrue(held.readOnly)
        assertEquals(
            listOf("tasks:create", "tasks:update", "tasks:delete"),
            withheld(held, MANIFEST),
        )
    }

    private fun fail(why: String): Nothing = throw AssertionError(why)
}
