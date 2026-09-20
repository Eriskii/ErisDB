package dev.erisdb.tasks

import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** The read path: a snapshot once, then the change feed. */
class SyncTest {

    private val facet = "tasks"

    private suspend fun FakeCore.add(title: String): String {
        val r = request(
            "POST", "/v1/items",
            JSONObject().put("facet", facet)
                .put("body", JSONObject().put("title", title).put("done", false))
                .toString(),
        )
        return r.getJSONObject("body").getString("id")
    }

    private fun titles(cache: Map<String, JSONObject>) =
        cache.values.map { it.getJSONObject("body").getString("title") }.sorted()

    // ------------------------------------------------------- the snapshot

    @Test
    fun aStoreLargerThanOnePageArrivesWhole() = runTest {
        val core = FakeCore()
        repeat(ITEM_PAGE + 250) { core.add("task $it") }

        val read = fetchAllItems(core, facet)

        // The bug: one `limit=1000` read and everything past the
        // thousandth item silently vanishes from the app.
        assertTrue(read is Read.Ok)
        assertEquals(ITEM_PAGE + 250, (read as Read.Ok).value.size)
        assertTrue(core.callsMatching("GET /v1/items?").size > 1)
    }

    @Test
    fun anUnreadableStoreSaysWhyRatherThanLookingEmpty() = runTest {
        val core = FakeCore().apply { add("one"); offline = true }
        assertTrue(fetchAllItems(core, facet) is Read.Failed)
    }

    // ------------------------------------------------------- the feed

    @Test
    fun theSeedStartsAtTheFeedHeadAndHoldsEveryItem() = runTest {
        val core = FakeCore()
        repeat(3) { core.add("task $it") }

        val seeded = seed(core, facet)
        assertTrue(seeded is Read.Ok)
        val value = (seeded as Read.Ok).value
        assertEquals(3, value.items.size)

        // Nothing new has happened, so a poll from the seeded cursor is
        // empty rather than replaying the store's whole history.
        val cache = value.items.toMutableMap()
        val polled = pollChanges(core, facet, value.cursor, cache)
        assertEquals(Read.Ok(value.cursor), polled)
        assertEquals(3, cache.size)
    }

    @Test
    fun laterWritesArriveAsDeltas() = runTest {
        val core = FakeCore()
        val kept = core.add("kept")
        val seeded = (seed(core, facet) as Read.Ok).value
        val cache = seeded.items.toMutableMap()

        val doomed = core.add("doomed")
        core.request(
            "PUT", "/v1/items/$kept",
            JSONObject().put("revision", 1L)
                .put("body", JSONObject("""{"title":"kept, edited","done":false}"""))
                .toString(),
        )
        core.request("DELETE", "/v1/items/$doomed")

        val cursor = pollChanges(core, facet, seeded.cursor, cache)
        assertTrue(cursor is Read.Ok)
        assertEquals(listOf("kept, edited"), titles(cache))

        // The whole store was read once, at seed time, and never again.
        assertEquals(1, core.callsMatching("GET /v1/items?").size)
    }

    @Test
    fun aFeedPageLargerThanOnePollIsCaughtUpInOneGo() = runTest {
        val core = FakeCore()
        val seeded = (seed(core, facet) as Read.Ok).value
        repeat(FEED_PAGE + 20) { core.add("task $it") }

        val cache = seeded.items.toMutableMap()
        val cursor = pollChanges(core, facet, seeded.cursor, cache)

        assertTrue(cursor is Read.Ok)
        assertEquals(FEED_PAGE + 20, cache.size)
    }

    @Test
    fun replayingAChangeLandsInTheSamePlace() = runTest {
        // A seed reads the head before the items, so the first poll may
        // re-deliver changes the snapshot already contains.
        val core = FakeCore()
        core.add("task")
        val cache = (fetchAllItems(core, facet) as Read.Ok).value.toMutableMap()
        val before = titles(cache)

        pollChanges(core, facet, 0L, cache)

        assertEquals(before, titles(cache))
    }

    @Test
    fun aLapsedRowIsNotAWrite() = runTest {
        val core = FakeCore()
        val id = core.add("water the plants")
        val seeded = (seed(core, facet) as Read.Ok).value
        val cache = seeded.items.toMutableMap()
        val updatedAt = cache[id]!!.getString("updated_at")

        core.lapse(id)
        pollChanges(core, facet, seeded.cursor, cache)

        // The core noticed a due date passing; nobody edited anything.
        assertEquals(updatedAt, cache[id]!!.getString("updated_at"))
        assertEquals(1L, cache[id]!!.getLong("revision"))
    }

    @Test
    fun aDeleteEmptiesTheCacheEntry() = runTest {
        val core = FakeCore()
        val id = core.add("task")
        val cache = (fetchAllItems(core, facet) as Read.Ok).value.toMutableMap()

        core.request("DELETE", "/v1/items/$id")
        pollChanges(core, facet, 0L, cache)

        assertEquals(emptyList<String>(), titles(cache))
    }

    @Test
    fun aFailedPollLeavesTheCursorWhereItWas() = runTest {
        val core = FakeCore()
        val seeded = (seed(core, facet) as Read.Ok).value
        core.offline = true

        val cache = seeded.items.toMutableMap()
        assertTrue(pollChanges(core, facet, seeded.cursor, cache) is Read.Failed)
    }
}
