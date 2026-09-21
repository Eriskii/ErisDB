package dev.erisdb.lists

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The permission grammar, and what this app does with the answer.
 *
 * `covers` is the same function the core runs, so a button this app shows
 * is a request the core will take, and a button it hides is one the core
 * would refuse. Getting the two out of step is the whole bug class this
 * file exists to close.
 */
class GrantsTest {

    // ------------------------------------------------------- the grammar

    @Test
    fun aGrantCoversTheExactPermissionItNames() {
        assertTrue(covers("lists:read", "lists:read"))
        assertFalse(covers("lists:read", "lists:create"))
        assertFalse(covers("lists:read", "tasks:read"))
    }

    @Test
    fun createAndUpdateAreSeparatePermissions() {
        // "can add but not edit" is a real state, so one never implies
        // the other.
        assertFalse(covers("lists:create", "lists:update"))
        assertFalse(covers("lists:update", "lists:create"))
    }

    @Test
    fun aTrailingStarTakesEveryRemainingSegment() {
        assertTrue(covers("lists:*", "lists:create"))
        assertTrue(covers("meta:*", "meta:pairing:approve"))
        assertTrue(covers("meta:pairing:*", "meta:pairing:approve"))
        assertTrue(covers("*", "lists:read"))
        assertTrue(covers("*", "meta:pairing:approve"))
    }

    @Test
    fun aTrailingStarNeedsSomethingToMatch() {
        // `*` as the final segment takes the rest, and there must be a rest.
        assertFalse(covers("meta:*", "meta"))
        assertFalse(covers("lists:*", "lists"))
    }

    @Test
    fun readEverythingIsNotAdministerEverything() {
        // The one the docs single out: `*:read` ends in the literal
        // `read`, so it matches exactly two segments and cannot reach a
        // three-segment meta permission.
        assertFalse(covers("*:read", "meta:facets:read"))
        assertFalse(covers("*:read", "meta:server:read"))
        assertFalse(covers("*:read", "meta:pairing:approve"))
        assertTrue(covers("*:read", "lists:read"))
        assertTrue(covers("*:read", "tasks:read"))
    }

    @Test
    fun aConcreteGrantDoesNotCoverAWildcard() {
        // Subsumption, not matching: the thing on the right is a pattern
        // too, and `read` is not `*`.
        assertFalse(covers("lists:read", "lists:*"))
        assertFalse(covers("meta:pairing:read", "meta:*"))
        assertTrue(covers("*", "lists:*"))
        assertTrue(covers("lists:*", "lists:*"))
    }

    @Test
    fun aStarInTheMiddleTakesExactlyOneSegment() {
        assertTrue(covers("meta:*:read", "meta:facets:read"))
        assertFalse(covers("meta:*:read", "meta:read"))
        assertFalse(covers("meta:*:read", "meta:a:b:read"))
    }

    @Test
    fun aGrantForANamespaceNobodyRegisteredIsInertRatherThanWrong() {
        assertTrue(covers("newthing:read", "newthing:read"))
        assertFalse(covers("newthing:read", "lists:read"))
    }

    // ------------------------------------------------------- the held set

    @Test
    fun aSetGrantsWhatAnyOneOfItsGrantsCovers() {
        val held = Grants(listOf("lists:read", "lists:create"))
        assertTrue(held.can("lists:read"))
        assertTrue(held.can("lists:create"))
        assertFalse(held.can("lists:update"))
        assertFalse(held.can("lists:delete"))
        assertFalse(held.can("meta:facets:write"))
    }

    @Test
    fun anEmptySetGrantsNothing() {
        val held = Grants(emptyList())
        assertFalse(held.can("lists:read"))
        assertFalse(held.can("lists:create"))
    }

    @Test
    fun grantsNotYetAskedForAreUnknownRatherThanAbsent() {
        // Before /v1/permissions has answered — offline on first launch,
        // say — the app shows its whole self and lets a 403 correct it,
        // rather than greying out a button on a guess.
        assertTrue(Grants.UNKNOWN.can("lists:create"))
        assertTrue(Grants.UNKNOWN.can("anything:at:all"))
        assertFalse("unknown is not read-only", Grants.UNKNOWN.readOnly)
    }

    @Test
    fun whatThisAppDoesToAnEntryRunsThroughTheFourActions() {
        val all = Grants(listOf("lists:*"))
        assertTrue(all.mayRead)
        assertTrue(all.mayCreate)
        assertTrue(all.mayUpdate)
        assertTrue(all.mayDelete)
        assertFalse(all.readOnly)

        val master = Grants(listOf("*"))
        assertTrue(master.mayDelete)
        assertTrue(master.can("meta:facets:write"))
    }

    @Test
    fun readAloneIsTheReadOnlyState() {
        val held = Grants(listOf("lists:read"))
        assertTrue(held.readOnly)
        assertTrue(held.mayRead)
        assertFalse(held.mayCreate)
        assertFalse(held.mayUpdate)
        assertFalse(held.mayDelete)
    }

    @Test
    fun canAddButNotEditIsNotReadOnly() {
        val held = Grants(listOf("lists:read", "lists:create"))
        assertFalse(held.readOnly)
        assertTrue(held.mayCreate)
        assertFalse(held.mayUpdate)
    }

    @Test
    fun aGrantForAnotherFacetDoesNothingHere() {
        val held = Grants(listOf("tasks:*"))
        assertFalse(held.mayRead)
        assertFalse(held.mayCreate)
        assertFalse(held.readOnly)
    }

    // ------------------------------------------------------- the manifest

    @Test
    fun theManifestIsTheFourActionsOverThisAppsFacet() {
        assertEquals(
            listOf("lists:read", "lists:create", "lists:update", "lists:delete"),
            MANIFEST,
        )
    }

    @Test
    fun theManifestDoesNotAskForGlobalFacetAdministration() {
        // meta:facets:write is register, change and remove *every* facet.
        // A lists app asking for it would be asking for the tasks app's
        // schema too. Its own create grant handles missing-schema setup.
        assertFalse("meta:facets:write" in MANIFEST)
        assertTrue(MANIFEST.none { it.startsWith("meta:") })
    }

    @Test
    fun everyGrantAskedForHasSomethingToSayToAHuman() {
        for (grant in MANIFEST) {
            assertFalse("$grant reads as itself", describeGrant(grant) == grant)
        }
        // Anything else shows the raw permission rather than inventing one.
        assertEquals("imap:sync", describeGrant("imap:sync"))
    }

    @Test
    fun whatWasNotGrantedIsNameableSoTheAppCanSaySo() {
        val held = Grants(listOf("lists:read", "lists:create"))
        assertEquals(listOf("lists:update", "lists:delete"), withheld(held, MANIFEST))
        assertEquals(emptyList<String>(), withheld(Grants(listOf("*")), MANIFEST))
        // Nothing is withheld from an app that has not asked yet.
        assertEquals(emptyList<String>(), withheld(Grants.UNKNOWN, MANIFEST))
    }

    // ------------------------------------------------------- storage

    @Test
    fun heldGrantsSurviveARestartAndAnUnaskedOneStaysUnknown() {
        val held = Grants(listOf("lists:read", "lists:update"))
        assertEquals(held.held, Grants(readGrants(writeGrants(held.held))).held)
        assertNull(writeGrants(null))
        assertNull(readGrants(null))
        // An empty set is a real answer — the operator granted nothing —
        // and must not read back as "never asked".
        assertEquals(emptyList<String>(), readGrants(writeGrants(emptyList())))
    }

    @Test
    fun aStoredValueThatIsNotAGrantListReadsAsUnknown() {
        assertNull(readGrants("{{{"))
    }

    // ------------------------------------------------------- asking

    @Test
    fun thePermissionsEndpointIsWhatTheTokenActuallyHolds() = runTest {
        val core = FakeCore().apply { grants = listOf("lists:read", "lists:create") }
        val read = fetchGrants(core)
        assertEquals(listOf("lists:read", "lists:create"), (read as Read.Ok).value.held)
        assertTrue("GET /v1/permissions" in core.calls)
    }

    @Test
    fun anUnreachableCoreLeavesTheLastKnownGrantsAlone() = runTest {
        val core = FakeCore().apply { grants = listOf("lists:*"); offline = true }
        assertTrue(fetchGrants(core) is Read.Failed)
    }

    @Test
    fun anApprovalNarrowerThanTheRequestIsWhatComesBack() = runTest {
        // The human pressed `s` and picked two of the four.
        val core = FakeCore().apply { grants = listOf("lists:read", "lists:update") }
        val held = (fetchGrants(core) as Read.Ok).value
        assertFalse(held.mayCreate)
        assertTrue(held.mayUpdate)
        assertEquals(listOf("lists:create", "lists:delete"), withheld(held, MANIFEST))
    }
}
