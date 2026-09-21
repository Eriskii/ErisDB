package dev.erisdb.tasks

import dev.erisdb.android.*

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
        assertTrue(covers("tasks:read", "tasks:read"))
        assertFalse(covers("tasks:read", "tasks:create"))
        assertFalse(covers("tasks:read", "lists:read"))
    }

    @Test
    fun createAndUpdateAreSeparatePermissions() {
        // "can add but not edit" is a real state, so one never implies
        // the other.
        assertFalse(covers("tasks:create", "tasks:update"))
        assertFalse(covers("tasks:update", "tasks:create"))
    }

    @Test
    fun aTrailingStarTakesEveryRemainingSegment() {
        assertTrue(covers("tasks:*", "tasks:create"))
        assertTrue(covers("meta:*", "meta:pairing:approve"))
        assertTrue(covers("meta:pairing:*", "meta:pairing:approve"))
        assertTrue(covers("*", "tasks:read"))
        assertTrue(covers("*", "meta:pairing:approve"))
    }

    @Test
    fun aTrailingStarNeedsSomethingToMatch() {
        // `*` as the final segment takes the rest, and there must be a rest.
        assertFalse(covers("meta:*", "meta"))
        assertFalse(covers("tasks:*", "tasks"))
    }

    @Test
    fun readEverythingIsNotAdministerEverything() {
        // The one the docs single out: `*:read` ends in the literal
        // `read`, so it matches exactly two segments and cannot reach a
        // three-segment meta permission.
        assertFalse(covers("*:read", "meta:facets:read"))
        assertFalse(covers("*:read", "meta:server:read"))
        assertFalse(covers("*:read", "meta:pairing:approve"))
        assertTrue(covers("*:read", "tasks:read"))
        assertTrue(covers("*:read", "lists:read"))
    }

    @Test
    fun aConcreteGrantDoesNotCoverAWildcard() {
        // Subsumption, not matching: the thing on the right is a pattern
        // too, and `read` is not `*`.
        assertFalse(covers("tasks:read", "tasks:*"))
        assertFalse(covers("meta:pairing:read", "meta:*"))
        assertTrue(covers("*", "tasks:*"))
        assertTrue(covers("tasks:*", "tasks:*"))
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
        assertFalse(covers("newthing:read", "tasks:read"))
    }

    // ------------------------------------------------------- the held set

    @Test
    fun aSetGrantsWhatAnyOneOfItsGrantsCovers() {
        val held = Grants(listOf("tasks:read", "tasks:create"))
        assertTrue(held.can("tasks:read"))
        assertTrue(held.can("tasks:create"))
        assertFalse(held.can("tasks:update"))
        assertFalse(held.can("tasks:delete"))
        assertFalse(held.can("meta:facets:write"))
    }

    @Test
    fun anEmptySetGrantsNothing() {
        val held = Grants(emptyList())
        assertFalse(held.can("tasks:read"))
        assertFalse(held.can("tasks:create"))
    }

    @Test
    fun grantsNotYetAskedForAreUnknownRatherThanAbsent() {
        // Before /v1/permissions has answered — offline on first launch,
        // say — the app shows its whole self and lets a 403 correct it,
        // rather than greying out a button on a guess.
        assertTrue(Grants.UNKNOWN.can("tasks:create"))
        assertTrue(Grants.UNKNOWN.can("anything:at:all"))
        assertFalse("unknown is not read-only", Grants.UNKNOWN.readOnly)
    }

    @Test
    fun whatThisAppDoesToATaskRunsThroughTheFourActions() {
        val all = Grants(listOf("tasks:*"))
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
        val held = Grants(listOf("tasks:read"))
        assertTrue(held.readOnly)
        assertTrue(held.mayRead)
        assertFalse(held.mayCreate)
        assertFalse(held.mayUpdate)
        assertFalse(held.mayDelete)
    }

    @Test
    fun canAddButNotEditIsNotReadOnly() {
        val held = Grants(listOf("tasks:read", "tasks:create"))
        assertFalse(held.readOnly)
        assertTrue(held.mayCreate)
        assertFalse(held.mayUpdate)
    }

    @Test
    fun aGrantForAnotherFacetDoesNothingHere() {
        val held = Grants(listOf("lists:*"))
        assertFalse(held.mayRead)
        assertFalse(held.mayCreate)
        assertFalse(held.readOnly)
    }

    // ------------------------------------------------------- the manifest

    @Test
    fun theManifestIsTheFourActionsOverThisAppsFacet() {
        assertEquals(
            listOf("tasks:read", "tasks:create", "tasks:update", "tasks:delete"),
            MANIFEST,
        )
    }

    @Test
    fun theManifestDoesNotAskForGlobalFacetAdministration() {
        // meta:facets:write is register, change and remove *every* facet.
        // A tasks app asking for it would be asking for the lists app's
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
        val held = Grants(listOf("tasks:read", "tasks:create"))
        assertEquals(listOf("tasks:update", "tasks:delete"), withheld(held, MANIFEST))
        assertEquals(emptyList<String>(), withheld(Grants(listOf("*")), MANIFEST))
        // Nothing is withheld from an app that has not asked yet.
        assertEquals(emptyList<String>(), withheld(Grants.UNKNOWN, MANIFEST))
    }

    // ------------------------------------------------------- storage

    @Test
    fun heldGrantsSurviveARestartAndAnUnaskedOneStaysUnknown() {
        val held = Grants(listOf("tasks:read", "tasks:update"))
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
}
