package dev.erisdb.tasks

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test
import java.time.Instant

/** Which tasks are worth interrupting someone for, and how often. */
class NotificationsTest {

    private fun ms(s: String) = Instant.parse(s).toEpochMilli()
    private val now = ms("2026-08-20T12:00:00Z")

    private fun item(id: String, revision: Long, body: String) = JSONObject()
        .put("id", id).put("revision", revision).put("body", JSONObject(body))

    private val overdue = item("a", 4L, """{"title":"bins","done":false,"due":"2026-08-19T20:00:00Z"}""")
    private val later = item("b", 1L, """{"title":"call","done":false,"due":"2026-08-30T09:00:00Z"}""")
    private val closed = item("c", 2L, """{"title":"done","done":true,"due":"2026-08-01T09:00:00Z"}""")
    private val undated = item("d", 1L, """{"title":"someday","done":false}""")

    private fun titles(items: List<JSONObject>) =
        items.map { it.getJSONObject("body").getString("title") }

    @Test
    fun onlyAnOpenTaskPastItsDueDateIsAnnounced() {
        val due = dueNow(listOf(overdue, later, closed, undated), emptyMap(), now)
        assertEquals(listOf("bins"), titles(due))
    }

    @Test
    fun aTaskIsAnnouncedOncePerRevisionNotOncePerPoll() {
        val items = listOf(overdue)
        val first = dueNow(items, emptyMap(), now)
        assertEquals(listOf("bins"), titles(first))

        val ledger = withAnnounced(emptyMap(), first, items.map { it.getString("id") }.toSet())
        assertEquals(emptyList<String>(), titles(dueNow(items, ledger, now)))
    }

    @Test
    fun editingAnOverdueTaskAnnouncesItAgain() {
        val ledger = mapOf("a" to 4L)
        val edited = item("a", 5L, """{"title":"bins, urgently","done":false,"due":"2026-08-19T20:00:00Z"}""")
        assertEquals(listOf("bins, urgently"), titles(dueNow(listOf(edited), ledger, now)))
    }

    @Test
    fun aDeletedTaskLetsGoOfItsSlot() {
        val ledger = mapOf("a" to 4L, "gone" to 1L)
        assertEquals(setOf("a"), withAnnounced(ledger, emptyList(), setOf("a")).keys)
    }
}
