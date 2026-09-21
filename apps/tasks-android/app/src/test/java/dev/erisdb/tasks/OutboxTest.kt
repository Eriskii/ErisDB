package dev.erisdb.tasks

import dev.erisdb.android.*
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Pure cache projection and bounded alias bookkeeping. Network behavior is
 * covered against the real APK, JNI, Iroh and PostgreSQL in tests/android. */
class OutboxTest {
    @Test
    fun aliasesAreBoundedByTheMostRecentCreates() {
        var aliases = mapOf<String, String>()
        for (i in 0 until ALIAS_LIMIT + 10) aliases = remembered(aliases, "pending-$i", "item-$i")
        assertEquals(ALIAS_LIMIT, aliases.size)
        assertEquals("item-${ALIAS_LIMIT + 9}", aliases["pending-${ALIAS_LIMIT + 9}"])
        assertNull(aliases["pending-0"])
    }

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

fun createOp(tmp: String, body: String): JSONObject = JSONObject()
    .put("op", "create").put("tmp", tmp).put("body", JSONObject(body))

fun updateOp(id: String, revision: Long, body: String): JSONObject = JSONObject()
    .put("op", "update").put("id", id).put("revision", revision).put("body", JSONObject(body))

fun deleteOp(id: String): JSONObject = JSONObject().put("op", "delete").put("id", id)
