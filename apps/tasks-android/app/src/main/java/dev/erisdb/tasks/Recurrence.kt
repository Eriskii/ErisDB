package dev.erisdb.tasks

import java.time.OffsetDateTime

/** The repeat periods the facet's schema admits. */
enum class RepeatUnit(val wire: String) {
    DAY("day"),
    WEEK("week"),
    MONTH("month"),
    YEAR("year");

    companion object {
        /** The unit for a schema value, or null when the string is not one. */
        fun fromWire(s: String?): RepeatUnit? = entries.firstOrNull { it.wire == s }
    }
}

/** `due` plus `k` periods of `n × unit`, anchored on the original due date
 * so month and year steps keep their day-of-month across clamped months
 * (Jan 31 → Feb 28 → Mar 31, never Feb 28 → Mar 28). */
private fun step(due: OffsetDateTime, n: Int, unit: RepeatUnit, k: Long): OffsetDateTime {
    val total = n.toLong() * k
    return when (unit) {
        RepeatUnit.DAY -> due.plusDays(total)
        RepeatUnit.WEEK -> due.plusWeeks(total)
        RepeatUnit.MONTH -> due.plusMonths(total)
        RepeatUnit.YEAR -> due.plusYears(total)
    }
}

/**
 * The next due date after completing a repeating task at `now`: `due`
 * advanced by whole `n × unit` periods until it is strictly after `now`.
 *
 * The step always happens at least once, so completing early still moves
 * the task on to the next period rather than leaving it due today. A due
 * date far in the past catches up in a single call — the task lands on
 * the next period boundary in the future, not on every one it missed.
 *
 * Month and year arithmetic clamps to the end of a short month: Jan 31
 * plus one month is Feb 28 (Feb 29 in a leap year).
 */
fun advance(due: OffsetDateTime, n: Int, unit: RepeatUnit, now: OffsetDateTime): OffsetDateTime {
    require(n >= 1) { "repeat interval must be at least 1, got $n" }
    var k = 1L
    var next = step(due, n, unit, k)
    while (!next.isAfter(now)) {
        k += 1
        next = step(due, n, unit, k)
    }
    return next
}

/** The repeat rule in words: "every day", "every 2 weeks". */
fun describe(n: Int, unit: RepeatUnit): String =
    if (n == 1) "every ${unit.wire}" else "every $n ${unit.wire}s"
