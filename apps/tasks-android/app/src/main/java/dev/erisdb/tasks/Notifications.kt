package dev.erisdb.tasks

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.time.ZoneId

// A due date that passes with the task still open is worth interrupting
// someone for. The core notices it too — the facet's `lapse` rule makes it
// emit a `lapsed` change — but the phone's own clock is the faster of the
// two, and both land in the same place: a task is announced once per
// revision, however it was noticed.

const val DUE_CHANNEL = "due"

/**
 * The tasks that have come due and have not been announced as they now
 * stand. Keyed on revision, so editing a task announces it again and
 * polling ten times does not.
 */
fun dueNow(
    items: Collection<JSONObject>,
    announced: Map<String, Long>,
    nowMs: Long,
): List<JSONObject> = items.filter {
    isOverdue(it.getJSONObject("body"), nowMs) &&
        announced[it.getString("id")] != it.optLong("revision")
}

/** The announcement ledger with `items` recorded, and items no longer in
 * the cache forgotten — a deleted task must not hold a slot forever. */
fun withAnnounced(
    announced: Map<String, Long>,
    items: Collection<JSONObject>,
    live: Set<String>,
): Map<String, Long> {
    val next = LinkedHashMap(announced.filterKeys { it in live })
    for (item in items) next[item.getString("id")] = item.optLong("revision")
    return next
}

/** The channel the due notices arrive on. Idempotent; safe every launch. */
fun ensureDueChannel(ctx: Context) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val channel = NotificationChannel(
        DUE_CHANNEL,
        "Due tasks",
        NotificationManager.IMPORTANCE_DEFAULT,
    ).apply { description = "A task's due date has passed with it still open." }
    ctx.getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
}

/** True when this app may post notifications at all. */
fun mayNotify(ctx: Context): Boolean =
    (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
        ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) ==
        PackageManager.PERMISSION_GRANTED) &&
        NotificationManagerCompat.from(ctx).areNotificationsEnabled()

/** Announce one overdue task. Tapping it opens the app. */
fun notifyDue(ctx: Context, item: JSONObject, nowMs: Long) {
    if (!mayNotify(ctx)) return
    val body = item.getJSONObject("body")
    val open = PendingIntent.getActivity(
        ctx,
        0,
        Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP),
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val when_ = dueOf(body)?.let { formatDue(it, nowMs, ZoneId.systemDefault()) }
    val notice = NotificationCompat.Builder(ctx, DUE_CHANNEL)
        .setSmallIcon(R.drawable.ic_notify)
        .setContentTitle(body.optString("title"))
        .setContentText(when_?.let { "due $it" } ?: "due")
        .setAutoCancel(true)
        .setContentIntent(open)
        .build()
    NotificationManagerCompat.from(ctx).notify(item.getString("id").hashCode(), notice)
}
