package dev.erisdb.tasks

import org.junit.Assert.assertEquals
import org.junit.Test
import java.time.OffsetDateTime

/** The recurrence rule: completing a repeating task advances its due date
 * past now, however many periods have been missed. */
class RecurrenceTest {

    private fun t(s: String) = OffsetDateTime.parse(s)

    @Test
    fun dailyAdvancesOnePeriod() {
        val due = t("2026-08-20T09:00:00Z")
        val now = t("2026-08-20T14:00:00Z")
        assertEquals(t("2026-08-21T09:00:00Z"), advance(due, 1, RepeatUnit.DAY, now))
    }

    @Test
    fun catchesUpOverManyMissedPeriods() {
        // Due in January, completed in August: one call lands in the future.
        val due = t("2026-01-05T08:00:00Z")
        val now = t("2026-08-20T12:00:00Z")
        val next = advance(due, 1, RepeatUnit.DAY, now)
        assertEquals(t("2026-08-21T08:00:00Z"), next)
    }

    @Test
    fun monthEndClamps() {
        // Jan 31 + 1 month is Feb 28 in a common year.
        val due = t("2026-01-31T10:00:00Z")
        val now = t("2026-02-01T00:00:00Z")
        assertEquals(t("2026-02-28T10:00:00Z"), advance(due, 1, RepeatUnit.MONTH, now))
    }

    @Test
    fun monthEndClampsInLeapYear() {
        val due = t("2028-01-31T10:00:00Z")
        val now = t("2028-02-01T00:00:00Z")
        assertEquals(t("2028-02-29T10:00:00Z"), advance(due, 1, RepeatUnit.MONTH, now))
    }

    @Test
    fun weeklyEveryTwoWeeksStepsInWholePeriods() {
        // Five weeks past due at n=2: the landing spot is due + 6 weeks,
        // never an off-cycle date.
        val due = t("2026-07-01T07:30:00Z")
        val now = t("2026-08-05T00:00:00Z")
        assertEquals(t("2026-08-12T07:30:00Z"), advance(due, 2, RepeatUnit.WEEK, now))
    }

    @Test
    fun alwaysAdvancesAtLeastOnce() {
        // Completing early still moves the task on to the next period.
        val due = t("2026-08-25T09:00:00Z")
        val now = t("2026-08-20T09:00:00Z")
        assertEquals(t("2026-08-26T09:00:00Z"), advance(due, 1, RepeatUnit.DAY, now))
    }

    @Test
    fun yearlyAdvances() {
        val due = t("2020-03-15T06:00:00Z")
        val now = t("2026-08-20T00:00:00Z")
        assertEquals(t("2027-03-15T06:00:00Z"), advance(due, 1, RepeatUnit.YEAR, now))
    }

    @Test
    fun preservesOffset() {
        val due = t("2026-08-20T09:00:00+02:00")
        val now = t("2026-08-20T12:00:00+02:00")
        assertEquals(t("2026-08-21T09:00:00+02:00"), advance(due, 1, RepeatUnit.DAY, now))
    }

    @Test
    fun describesSingularAndPlural() {
        assertEquals("every day", describe(1, RepeatUnit.DAY))
        assertEquals("every 2 weeks", describe(2, RepeatUnit.WEEK))
        assertEquals("every month", describe(1, RepeatUnit.MONTH))
        assertEquals("every 3 years", describe(3, RepeatUnit.YEAR))
    }

    @Test
    fun unitsRoundTripThroughTheirWireNames() {
        assertEquals(RepeatUnit.DAY, RepeatUnit.fromWire("day"))
        assertEquals(RepeatUnit.WEEK, RepeatUnit.fromWire("week"))
        assertEquals(RepeatUnit.MONTH, RepeatUnit.fromWire("month"))
        assertEquals(RepeatUnit.YEAR, RepeatUnit.fromWire("year"))
        assertEquals(null, RepeatUnit.fromWire("fortnight"))
        assertEquals("week", RepeatUnit.WEEK.wire)
    }
}
