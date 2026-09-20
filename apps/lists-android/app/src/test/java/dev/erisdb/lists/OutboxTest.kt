package dev.erisdb.lists

import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The write path: ordering, pending ids, and one send per op. */
class OutboxTest {

    private val facet = "lists"

    private fun names(core: FakeCore) =
        core.items.values.map { it.getJSONObject("body").getString("name") }

    // ------------------------------------------------------- pending ids

    @Test
    fun deletingAStillQueuedCreateLeavesNothingBehind() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"mistake"}"""))
        box.enqueue(deleteOp("pending-x"))

        assertNull(drainOutbox(box, core, facet))

        // The create made a real item and the delete followed it there,
        // instead of 404ing against the pending id and being called done.
        assertEquals(emptyList<String>(), names(core))
        assertTrue("DELETE /v1/items/item-0" in core.calls)
    }

    @Test
    fun editingAStillQueuedCreateKeepsTheEdit() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"draft"}"""))
        box.enqueue(updateOp("pending-x", 0L, """{"list":"reading","name":"final"}"""))

        assertNull(drainOutbox(box, core, facet))

        assertEquals(listOf("final"), names(core))
    }

    @Test
    fun anEditMadeAfterTheCreateHasAlreadyDrainedStillLands() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"draft"}"""))
        assertNull(drainOutbox(box, core, facet))

        // The editor is still holding the pending item, so the edit the
        // user makes a moment later names the id that no longer exists.
        box.enqueue(updateOp("pending-x", 0L, """{"list":"reading","name":"second thoughts"}"""))
        assertNull(drainOutbox(box, core, facet))

        assertEquals(listOf("second thoughts"), names(core))
    }

    @Test
    fun aRejectedCreateTakesItsFollowersWithIt() = runTest {
        val core = FakeCore(facet = "other")   // nothing this app writes lands
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"doomed"}"""))
        box.enqueue(updateOp("pending-x", 0L, """{"list":"reading","name":"doomed still"}"""))

        drainOutbox(box, core, facet)

        // No PUT was ever sent against a pending id.
        assertEquals(emptyList<String>(), core.callsMatching("PUT /v1/items/pending-"))
        assertEquals(emptyList<JSONObject>(), box.ops())
    }

    @Test
    fun aliasesAreBoundedByTheMostRecentCreates() {
        var aliases = mapOf<String, String>()
        for (i in 0 until ALIAS_LIMIT + 10) aliases = remembered(aliases, "pending-$i", "item-$i")
        assertEquals(ALIAS_LIMIT, aliases.size)
        assertEquals("item-${ALIAS_LIMIT + 9}", aliases["pending-${ALIAS_LIMIT + 9}"])
        assertNull(aliases["pending-0"])
    }

    // ------------------------------------------------------- refusals

    @Test
    fun aForbiddenCreateLeavesTheQueueInsteadOfRetryingForever() = runTest {
        // The human approved less than this app asked for. Nothing about
        // that changes on the next poll, so the op goes with a reason.
        val core = FakeCore().apply { forbidden = setOf("create") }
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"not mine to add"}"""))

        val reason = drainOutbox(box, core, facet)

        assertEquals(emptyList<JSONObject>(), box.ops())
        assertTrue(reason!!.contains("lists:create"))
        assertEquals(1, core.callsMatching("POST /v1/items").size)
    }

    @Test
    fun aForbiddenUpdateIsDroppedRatherThanRepeated() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"mine"}"""))
        assertNull(drainOutbox(box, core, facet))

        core.forbidden = setOf("update")
        box.enqueue(updateOp("item-0", 1L, """{"list":"reading","name":"mine, edited"}"""))
        val reason = drainOutbox(box, core, facet)

        assertEquals(emptyList<JSONObject>(), box.ops())
        assertTrue(reason!!.contains("lists:update"))
        // One attempt. A 403 is not a revision conflict, so nothing refetches.
        assertEquals(1, core.callsMatching("PUT /v1/items/item-0").size)
    }

    @Test
    fun aForbiddenDeleteIsDroppedRatherThanRepeated() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"stays"}"""))
        assertNull(drainOutbox(box, core, facet))

        core.forbidden = setOf("delete")
        box.enqueue(deleteOp("item-0"))
        val reason = drainOutbox(box, core, facet)

        assertEquals(emptyList<JSONObject>(), box.ops())
        assertTrue(reason!!.contains("lists:delete"))
        assertEquals(listOf("stays"), names(core))
    }

    @Test
    fun aFacetNobodyRegisteredIsTheOperatorsJobAndSaysSo() = runTest {
        // This app does not ask for meta:facets:write, so it cannot
        // register the facet itself — and says who can.
        val core = FakeCore().apply { facetRegistered = false }
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"list":"reading","name":"nowhere to go"}"""))

        val reason = drainOutbox(box, core, facet)

        assertEquals(emptyList<JSONObject>(), box.ops())
        assertTrue(reason!!.contains("not registered"))
        assertTrue(reason.contains("operator"))
    }

    // ------------------------------------------------------- ordering

    @Test
    fun aTransportFailureStopsTheDrainWhereItStands() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-a", """{"list":"reading","name":"first"}"""))
        box.enqueue(createOp("pending-b", """{"list":"reading","name":"second"}"""))
        core.offline = true

        assertNull(drainOutbox(box, core, facet))
        assertEquals(2, box.ops().size)

        core.offline = false
        assertNull(drainOutbox(box, core, facet))
        assertEquals(listOf("first", "second"), names(core))
    }

    @Test
    fun anOpQueuedWhileAnotherIsInFlightIsNotErased() = runTest {
        val core = FakeCore()
        val box = object : OutboxStore {
            val inner = FakeOutbox()
            var sends = 0
            override fun ops() = inner.ops()
            override fun writeOps(ops: List<JSONObject>) = inner.writeOps(ops)
            override fun aliases() = inner.aliases()
            override fun writeAliases(aliases: Map<String, String>) = inner.writeAliases(aliases)
        }
        box.inner.enqueue(createOp("pending-a", """{"list":"reading","name":"first"}"""))

        // The user taps mid-flight: the second op is appended while the
        // first is still being sent.
        val slow = object : CoreApi {
            override suspend fun request(method: String, path: String, body: String?): JSONObject {
                if (box.sends++ == 0) {
                    box.inner.enqueue(createOp("pending-b", """{"list":"reading","name":"second"}"""))
                }
                return core.request(method, path, body)
            }
        }
        drainOutbox(box, slow, facet)

        assertEquals(listOf("first", "second"), names(core))
    }

    // ------------------------------------------------------- the gate

    @Test
    fun twoConcurrentSyncsSendEachOpExactlyOnce() = runTest {
        val core = FakeCore().apply { latencyMs = 50 }
        val box = FakeOutbox()
        box.enqueue(createOp("pending-a", """{"list":"reading","name":"only once"}"""))
        box.enqueue(createOp("pending-b", """{"list":"reading","name":"also once"}"""))
        val gate = SyncGate()

        // The ten-second poll and a user mutation, arriving together.
        listOf(
            async { gate.serialized { drainOutbox(box, core, facet) } },
            async { gate.serialized { drainOutbox(box, core, facet) } },
        ).awaitAll()

        assertEquals(2, core.callsMatching("POST /v1/items").size)
        assertEquals(listOf("only once", "also once"), names(core))
    }

    @Test
    fun anUngatedDrainIsWhatTheGateIsFor() = runTest {
        // The bug the gate closes: two drains reading the same queue send
        // the same op twice and the core takes both writes.
        val core = FakeCore().apply { latencyMs = 50 }
        val box = FakeOutbox()
        box.enqueue(createOp("pending-a", """{"list":"reading","name":"duplicated"}"""))

        listOf(
            async { drainOutbox(box, core, facet) },
            async { drainOutbox(box, core, facet) },
        ).awaitAll()

        assertEquals(2, core.callsMatching("POST /v1/items").size)
    }

    // ------------------------------------------------------- optimism

    @Test
    fun queuedOpsShowOnTopOfTheSnapshot() {
        val server = listOf(
            JSONObject().put("id", "item-0").put("revision", 3L)
                .put("body", JSONObject("""{"list":"reading","name":"kept"}""")),
            JSONObject().put("id", "item-1").put("revision", 1L)
                .put("body", JSONObject("""{"list":"reading","name":"doomed"}""")),
        )
        val ops = listOf(
            createOp("pending-x", """{"list":"reading","name":"new"}"""),
            updateOp("item-0", 3L, """{"list":"reading","name":"kept, edited"}"""),
            deleteOp("item-1"),
        )
        val shown = applyPending(server, ops, emptyMap())

        assertEquals(
            listOf("kept, edited", "new"),
            shown.map { it.getJSONObject("body").getString("name") },
        )
    }

    @Test
    fun aResolvedCreateShowsOnceUnderItsRealId() {
        // The create has landed and the feed has already delivered the
        // item, but the create is still in the queue for one more moment.
        val server = listOf(
            JSONObject().put("id", "item-0").put("revision", 1L)
                .put("body", JSONObject("""{"list":"reading","name":"landed"}""")),
        )
        val ops = listOf(createOp("pending-x", """{"list":"reading","name":"landed"}"""))
        val shown = applyPending(server, ops, mapOf("pending-x" to "item-0"))

        assertEquals(listOf("item-0"), shown.map { it.getString("id") })
    }
}
