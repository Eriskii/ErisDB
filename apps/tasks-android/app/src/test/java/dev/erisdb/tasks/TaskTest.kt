package dev.erisdb.tasks

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.time.Instant
import java.time.OffsetDateTime
import java.time.ZoneId

/** The JSON skin: task bodies in, task bodies out, recurrence underneath. */
class TaskTest {

    private fun ms(s: String) = Instant.parse(s).toEpochMilli()

    private fun task(json: String) = JSONObject(json)

    @Test
    fun completingAOneOffMarksItDone() {
        val body = task("""{"title":"post the letter","done":false}""")
        val next = completedBody(body, ms("2026-08-20T12:00:00Z"))
        assertTrue(next.getBoolean("done"))
        assertEquals("post the letter", next.getString("title"))
    }

    @Test
    fun completingARepeaterRollsTheDueDateForward() {
        val body = task(
            """{"title":"water the plants","done":false,"due":"2026-08-18T09:00:00Z",
                "repeat":{"n":2,"unit":"day"}}"""
        )
        val next = completedBody(body, ms("2026-08-20T12:00:00Z"))
        assertFalse(next.getBoolean("done"))
        assertEquals("2026-08-22T09:00:00Z", next.getString("due"))
    }

    @Test
    fun aRepeaterWithNoDueDateJustCloses() {
        val body = task("""{"title":"whenever","done":false,"repeat":{"n":1,"unit":"week"}}""")
        val next = completedBody(body, ms("2026-08-20T12:00:00Z"))
        assertTrue(next.getBoolean("done"))
    }

    @Test
    fun completingLeavesTheRestOfTheBodyAlone() {
        val body = task(
            """{"title":"bins","done":false,"notes":"green one","due":"2026-08-18T20:00:00Z",
                "repeat":{"n":1,"unit":"week"}}"""
        )
        val next = completedBody(body, ms("2026-08-20T12:00:00Z"))
        assertEquals("green one", next.getString("notes"))
        assertEquals("week", next.getJSONObject("repeat").getString("unit"))
        // The original is untouched — completion returns a new body.
        assertEquals("2026-08-18T20:00:00Z", body.getString("due"))
    }

    @Test
    fun togglingADoneTaskReopensIt() {
        val body = task("""{"title":"done thing","done":true}""")
        assertFalse(toggledBody(body, ms("2026-08-20T12:00:00Z")).getBoolean("done"))
    }

    @Test
    fun dueValuesAreWrittenAsUtcInstants() {
        val body = task(
            """{"title":"call","done":false,"due":"2026-08-19T09:00:00+02:00",
                "repeat":{"n":1,"unit":"day"}}"""
        )
        val next = completedBody(body, ms("2026-08-20T12:00:00Z"))
        // 09:00+02:00 is 07:00Z; the next daily beat is the 21st.
        assertEquals("2026-08-21T07:00:00Z", next.getString("due"))
    }

    @Test
    fun aMissingOrJunkDueReadsAsNoDue() {
        assertNull(dueOf(task("""{"title":"x","done":false}""")))
        assertNull(dueOf(task("""{"title":"x","done":false,"due":"soonish"}""")))
    }

    @Test
    fun anIncompleteRepeatRuleReadsAsNoRepeat() {
        assertNull(repeatOf(task("""{"title":"x","done":false}""")))
        assertNull(repeatOf(task("""{"title":"x","done":false,"repeat":{"n":1,"unit":"aeon"}}""")))
        assertNull(repeatOf(task("""{"title":"x","done":false,"repeat":{"n":0,"unit":"day"}}""")))
        assertEquals(3 to RepeatUnit.MONTH,
            repeatOf(task("""{"title":"x","done":false,"repeat":{"n":3,"unit":"month"}}""")))
    }

    @Test
    fun overdueIsOnlyEverTrueForOpenTasks() {
        val past = """"due":"2026-08-01T00:00:00Z""""
        assertTrue(isOverdue(task("""{"title":"x","done":false,$past}"""), ms("2026-08-20T12:00:00Z")))
        assertFalse(isOverdue(task("""{"title":"x","done":true,$past}"""), ms("2026-08-20T12:00:00Z")))
        assertFalse(isOverdue(task("""{"title":"x","done":false}"""), ms("2026-08-20T12:00:00Z")))
    }

    @Test
    fun dueDatesReadInDayScale() {
        val utc = ZoneId.of("UTC")
        val now = ms("2026-08-20T12:00:00Z")
        fun f(s: String) = formatDue(OffsetDateTime.parse(s), now, utc)
        assertEquals("today 14:00", f("2026-08-20T14:00:00Z"))
        assertEquals("tomorrow 09:00", f("2026-08-21T09:00:00Z"))
        assertEquals("yesterday 18:00", f("2026-08-19T18:00:00Z"))
        assertEquals("Tue 25 Aug 08:00", f("2026-08-25T08:00:00Z"))
        assertEquals("Fri 1 Jan 2027 00:00", f("2027-01-01T00:00:00Z"))
    }

    @Test
    fun readingOrderIsOverdueThenSoonThenUndatedThenDone() {
        val items = listOf(
            "done-old" to """{"title":"a","done":true,"due":"2026-01-01T00:00:00Z"}""",
            "undated" to """{"title":"b","done":false}""",
            "soon" to """{"title":"c","done":false,"due":"2026-08-21T00:00:00Z"}""",
            "overdue" to """{"title":"d","done":false,"due":"2026-08-01T00:00:00Z"}""",
        ).map { (id, body) -> JSONObject().put("id", id).put("body", JSONObject(body)) }

        assertEquals(
            listOf("overdue", "soon", "undated", "done-old"),
            taskOrder(items).map { it.getString("id") },
        )
    }
}
