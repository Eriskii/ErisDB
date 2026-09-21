package dev.erisdb.lists

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Two-phase pairing: redeem a code, then wait for a person.
 *
 * Collection is repeatable for the same installation. These unit tests
 * verify local storage ordering; the real E2E suite proves authorization.
 */
class RedeemTest {

    private fun core() = FakeCore()

    // ------------------------------------------------------------ redeem

    @Test
    fun redeemingSaysWhoTheAppIsAndWhatItWants() = runTest {
        val core = core()
        assertEquals(Approval.Waiting(), requestPairing(core, CLIENT, MANIFEST))

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
        core.offline = false
        val resumed = collect(core) { fail("nothing was approved") }
        assertEquals(Approval.Pending, resumed)
        assertTrue(pollingOn(resumed))
        assertEquals(Approval.Waiting(), requestPairing(core, CLIENT, MANIFEST))
    }

    @Test
    fun aCodeThisAppAlreadyRedeemedIsWaitedOnRatherThanRefused() = runTest {
        // The app was killed between redeeming and collecting. The session
        // knows where it stands, so the second run goes and asks it.
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        assertEquals(Approval.Waiting(), requestPairing(core, CLIENT, MANIFEST))
    }

    // ------------------------------------------------------------ collect

    @Test
    fun aRedeemedCodeWaitsUntilAPersonAnswers() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        assertEquals(Approval.Waiting(), collect(core) { fail("nothing to keep yet") })

        core.approve(listOf("lists:read", "lists:create"), "erisdb1.granted.sig")
        val out = collect(core) {}
        assertEquals("erisdb1.granted.sig", (out as Approval.Approved).token)
        assertEquals(listOf("lists:read", "lists:create"), out.granted)
    }

    @Test
    fun theCollectedTokenIsOnDiskBeforeCollectReturns() = runTest {
        // Persist before allowing app work to proceed.
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.approve(listOf("lists:*"), "erisdb1.only.chance")

        var kept: String? = null
        val out = collect(core) { kept = it }

        assertEquals("erisdb1.only.chance", kept)
        assertEquals("erisdb1.only.chance", (out as Approval.Approved).token)
        assertEquals("erisdb1.only.chance", core.pairToken)
    }

    @Test
    fun theSameInstallationCanRecoverALostCollectionResponse() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.approve(listOf("lists:read"), "erisdb1.recoverable")
        collect(core) {}
        val recovered = collect(core) {}
        assertEquals("erisdb1.recoverable", (recovered as Approval.Approved).token)
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
        // Transport failure and awaiting approval both keep polling.
        assertTrue(pollingOn(out))
        assertTrue(pollingOn(Approval.Waiting()))
        assertFalse(pollingOn(Approval.Denied))
        assertFalse(pollingOn(Approval.Over("gone")))
        assertFalse(pollingOn(Approval.Approved("erisdb1.x", emptyList())))
    }

    @Test
    fun anApprovalNarrowerThanTheRequestIsCarriedNotComplainedAbout() = runTest {
        val core = core()
        requestPairing(core, CLIENT, MANIFEST)
        core.approve(listOf("lists:read"), "erisdb1.narrow")

        val out = collect(core) {} as Approval.Approved
        val held = Grants(out.granted)
        assertTrue(held.readOnly)
        assertEquals(
            listOf("lists:create", "lists:update", "lists:delete"),
            withheld(held, MANIFEST),
        )
    }

    private fun fail(why: String): Nothing = throw AssertionError(why)
}
