package dev.erisdb.tasks

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

    private val facet = "tasks"

    private fun bodies(core: FakeCore) =
        core.items.values.map { it.getJSONObject("body").getString("title") }

    // ------------------------------------------------------- pending ids

    @Test
    fun deletingAStillQueuedCreateLeavesNothingBehind() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"title":"mistake","done":false}"""))
        box.enqueue(deleteOp("pending-x"))

        assertNull(drainOutbox(box, core, facet))

        // The create made a real item and the delete followed it there,
        // instead of 404ing against the pending id and being called done.
        assertEquals(emptyList<String>(), bodies(core))
        assertTrue("DELETE /v1/items/item-0" in core.calls)
    }

    @Test
    fun editingAStillQueuedCreateKeepsTheEdit() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"title":"draft","done":false}"""))
        box.enqueue(updateOp("pending-x", 0L, """{"title":"final","done":false}"""))

        assertNull(drainOutbox(box, core, facet))

        assertEquals(listOf("final"), bodies(core))
    }

    @Test
    fun anEditMadeAfterTheCreateHasAlreadyDrainedStillLands() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"title":"draft","done":false}"""))
        assertNull(drainOutbox(box, core, facet))

        // The editor is still holding the pending item, so the edit the
        // user makes a moment later names the id that no longer exists.
        box.enqueue(updateOp("pending-x", 0L, """{"title":"second thoughts","done":false}"""))
        assertNull(drainOutbox(box, core, facet))

        assertEquals(listOf("second thoughts"), bodies(core))
    }

    @Test
    fun aRejectedCreateTakesItsFollowersWithIt() = runTest {
        val core = FakeCore(facet = "other")   // nothing this app writes lands
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"title":"doomed","done":false}"""))
        box.enqueue(updateOp("pending-x", 0L, """{"title":"doomed still","done":false}"""))

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
        box.enqueue(createOp("pending-x", """{"title":"not mine to add","done":false}"""))

        val reason = drainOutbox(box, core, facet)

        assertEquals(emptyList<JSONObject>(), box.ops())
        assertTrue(reason!!.contains("tasks:create"))
        assertEquals(1, core.callsMatching("POST /v1/items").size)
    }

    @Test
    fun aForbiddenUpdateIsDroppedRatherThanRepeated() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"title":"mine","done":false}"""))
        assertNull(drainOutbox(box, core, facet))

        core.forbidden = setOf("update")
        box.enqueue(updateOp("item-0", 1L, """{"title":"mine, edited","done":false}"""))
        val reason = drainOutbox(box, core, facet)

        assertEquals(emptyList<JSONObject>(), box.ops())
        assertTrue(reason!!.contains("tasks:update"))
        // One attempt. A 403 is not a revision conflict, so nothing refetches.
        assertEquals(1, core.callsMatching("PUT /v1/items/item-0").size)
    }

    @Test
    fun aForbiddenDeleteIsDroppedRatherThanRepeated() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"title":"stays","done":false}"""))
        assertNull(drainOutbox(box, core, facet))

        core.forbidden = setOf("delete")
        box.enqueue(deleteOp("item-0"))
        val reason = drainOutbox(box, core, facet)

        assertEquals(emptyList<JSONObject>(), box.ops())
        assertTrue(reason!!.contains("tasks:delete"))
        assertEquals(listOf("stays"), bodies(core))
    }

    @Test
    fun aMissingFacetKeepsTheEntryQueuedUntilSetupSucceeds() = runTest {
        val core = FakeCore().apply { facetRegistered = false }
        val box = FakeOutbox()
        box.enqueue(createOp("pending-x", """{"title":"waiting for setup","done":false}"""))
        assertNull(drainOutbox(box, core, facet))
        assertEquals(1, box.ops().size)
        core.facetRegistered = true
        assertNull(drainOutbox(box, core, facet))
        assertTrue(box.ops().isEmpty())
    }

    // ------------------------------------------------------- ordering

    @Test
    fun aTransportFailureStopsTheDrainWhereItStands() = runTest {
        val core = FakeCore()
        val box = FakeOutbox()
        box.enqueue(createOp("pending-a", """{"title":"first","done":false}"""))
        box.enqueue(createOp("pending-b", """{"title":"second","done":false}"""))
        core.offline = true

        assertNull(drainOutbox(box, core, facet))
        assertEquals(2, box.ops().size)

        core.offline = false
        assertNull(drainOutbox(box, core, facet))
        assertEquals(listOf("first", "second"), bodies(core))
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
        box.inner.enqueue(createOp("pending-a", """{"title":"first","done":false}"""))

        // The user taps mid-flight: the second op is appended while the
        // first is still being sent.
        val slow = object : CoreApi {
            override suspend fun request(method: String, path: String, body: String?): JSONObject {
                if (box.sends++ == 0) {
                    box.inner.enqueue(createOp("pending-b", """{"title":"second","done":false}"""))
                }
                return core.request(method, path, body)
            }
        }
        drainOutbox(box, slow, facet)

        assertEquals(listOf("first", "second"), bodies(core))
    }

    // ------------------------------------------------------- the gate

    @Test
    fun twoConcurrentSyncsSendEachOpExactlyOnce() = runTest {
        val core = FakeCore().apply { latencyMs = 50 }
        val box = FakeOutbox()
        box.enqueue(createOp("pending-a", """{"title":"only once","done":false}"""))
        box.enqueue(createOp("pending-b", """{"title":"also once","done":false}"""))
        val gate = SyncGate()

        // The ten-second poll and a user mutation, arriving together.
        listOf(
            async { gate.serialized { drainOutbox(box, core, facet) } },
            async { gate.serialized { drainOutbox(box, core, facet) } },
        ).awaitAll()

        assertEquals(2, core.callsMatching("POST /v1/items").size)
        assertEquals(listOf("only once", "also once"), bodies(core))
    }

    @Test
    fun anUngatedDrainIsWhatTheGateIsFor() = runTest {
        // The bug the gate closes: two drains reading the same queue send
        // the same op twice and the core takes both writes.
        val core = FakeCore().apply { latencyMs = 50 }
        val box = FakeOutbox()
        box.enqueue(createOp("pending-a", """{"title":"duplicated","done":false}"""))

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
                .put("body", JSONObject("""{"title":"kept","done":false}""")),
            JSONObject().put("id", "item-1").put("revision", 1L)
                .put("body", JSONObject("""{"title":"doomed","done":false}""")),
        )
        val ops = listOf(
            createOp("pending-x", """{"title":"new","done":false}"""),
            updateOp("item-0", 3L, """{"title":"kept, edited","done":false}"""),
            deleteOp("item-1"),
        )
        val shown = applyPending(server, ops, emptyMap())

        assertEquals(
            listOf("kept, edited", "new"),
            shown.map { it.getJSONObject("body").getString("title") },
        )
    }

    @Test
    fun aResolvedCreateShowsOnceUnderItsRealId() {
        // The create has landed and the feed has already delivered the
        // item, but the create is still in the queue for one more moment.
        val server = listOf(
            JSONObject().put("id", "item-0").put("revision", 1L)
                .put("body", JSONObject("""{"title":"landed","done":false}""")),
        )
        val ops = listOf(createOp("pending-x", """{"title":"landed","done":false}"""))
        val shown = applyPending(server, ops, mapOf("pending-x" to "item-0"))

        assertEquals(listOf("item-0"), shown.map { it.getString("id") })
    }
}
