package dev.erisdb.tasks

import org.json.JSONObject
import java.time.Instant
import java.time.OffsetDateTime
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.time.temporal.ChronoUnit

// A thin JSON skin over Recurrence.kt: task bodies are
// {title, done, due?, notes?, repeat?} and due values are ISO 8601
// date-times, written back as UTC instants so every client reads the
// same string.

/** The task's due date, or null when it has none (or an unparseable one). */
fun dueOf(body: JSONObject): OffsetDateTime? {
    val raw = body.optString("due").ifEmpty { return null }
    return try {
        OffsetDateTime.parse(raw)
    } catch (_: Exception) {
        null
    }
}

/** The repeat rule as (n, unit), or null when the task is a one-off. */
fun repeatOf(body: JSONObject): Pair<Int, RepeatUnit>? {
    val r = body.optJSONObject("repeat") ?: return null
    val unit = RepeatUnit.fromWire(r.optString("unit")) ?: return null
    val n = r.optInt("n", 0)
    return if (n >= 1) n to unit else null
}

/** ISO 8601 in UTC — the form the whole tasks facet is written in. */
fun isoOf(t: OffsetDateTime): String = t.toInstant().toString()

/**
 * The body a task takes when the user completes it. A repeating task with
 * a due date rolls forward instead of closing: its due advances past now
 * and `done` stays false. Everything else simply becomes done.
 */
fun completedBody(body: JSONObject, nowMs: Long): JSONObject {
    val next = JSONObject(body.toString())
    val due = dueOf(body)
    val repeat = repeatOf(body)
    if (due != null && repeat != null) {
        val now = OffsetDateTime.ofInstant(Instant.ofEpochMilli(nowMs), due.offset)
        next.put("due", isoOf(advance(due, repeat.first, repeat.second, now)))
        next.put("done", false)
    } else {
        next.put("done", true)
    }
    return next
}

/** The body a task takes when the user un-checks it. */
fun uncompletedBody(body: JSONObject): JSONObject =
    JSONObject(body.toString()).put("done", false)

/** Flipping the checkbox: complete by the recurrence rule, or reopen. */
fun toggledBody(body: JSONObject, nowMs: Long): JSONObject =
    if (body.optBoolean("done")) uncompletedBody(body) else completedBody(body, nowMs)

/**
 * Reading order: open tasks with a due date first, soonest (so most
 * overdue) at the top; then open tasks with no due date, by title; then
 * the done ones, most recently due last.
 */
fun taskOrder(items: List<JSONObject>): List<JSONObject> =
    items.sortedWith(
        compareBy<JSONObject> { it.getJSONObject("body").optBoolean("done") }
            .thenBy { if (dueOf(it.getJSONObject("body")) == null) 1 else 0 }
            .thenBy { dueOf(it.getJSONObject("body"))?.toInstant() ?: Instant.EPOCH }
            .thenBy { it.getJSONObject("body").optString("title").lowercase() }
    )

private val timeFmt = DateTimeFormatter.ofPattern("HH:mm")
private val dayFmt = DateTimeFormatter.ofPattern("EEE d MMM")
private val yearDayFmt = DateTimeFormatter.ofPattern("EEE d MMM yyyy")

/** The due date in the reader's own day-scale: "today 14:00", "tomorrow
 * 09:00", "Mon 25 Aug 18:00". */
fun formatDue(due: OffsetDateTime, nowMs: Long, zone: ZoneId = ZoneId.systemDefault()): String {
    val local = due.atZoneSameInstant(zone)
    val now = Instant.ofEpochMilli(nowMs).atZone(zone)
    val days = ChronoUnit.DAYS.between(now.toLocalDate(), local.toLocalDate())
    val time = timeFmt.format(local)
    return when {
        days == 0L -> "today $time"
        days == 1L -> "tomorrow $time"
        days == -1L -> "yesterday $time"
        local.year != now.year -> "${yearDayFmt.format(local)} $time"
        else -> "${dayFmt.format(local)} $time"
    }
}

/** True when an open task's due date has passed. */
fun isOverdue(body: JSONObject, nowMs: Long): Boolean {
    if (body.optBoolean("done")) return false
    val due = dueOf(body) ?: return false
    return due.toInstant().toEpochMilli() <= nowMs
}
