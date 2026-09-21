package dev.erisdb.tasks

import dev.erisdb.client.ErisDB
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import java.net.URLEncoder

// The core's HTTP surface, and the two things every caller of it needs:
// a way to read an answer, and a way to be the only one asking.

/**
 * One blocking call to the core. The response envelope is
 * `{"status": n, "body": …}`, or `{"status": 0, "error": …}` when the
 * transport never got an answer.
 *
 * The sync engine speaks only this, so it runs on the JVM against a fake
 * core with no Android and no QUIC underneath it.
 */
interface CoreApi {
    suspend fun request(method: String, path: String, body: String? = null): JSONObject
}

/** The real one: erisdb-client over Iroh QUIC. Every call blocks. */
object ErisDBApi : CoreApi {
    override suspend fun request(method: String, path: String, body: String?): JSONObject =
        withContext(Dispatchers.IO) { ErisDB.request(method, path, body) }
}

/** A read from the core: the value, or why it did not arrive. */
sealed class Read<out T> {
    data class Ok<out T>(val value: T) : Read<T>()
    data class Failed(val error: String, val status: Int = 0) : Read<Nothing>()
}

/** True when the core never answered, as opposed to answering badly. */
fun transportDown(r: JSONObject): Boolean = r.optInt("status") == 0

/** The most specific thing the core said about a failure. */
fun why(r: JSONObject): String =
    r.optJSONObject("body")?.optString("detail")?.ifEmpty { null }
        ?: r.optString("error").ifEmpty { "status ${r.optInt("status")}" }

fun enc(s: String): String = URLEncoder.encode(s, "UTF-8")

fun jsonObjects(arr: JSONArray): List<JSONObject> =
    (0 until arr.length()).map { arr.getJSONObject(it) }

/**
 * Sync happens one at a time. The ten-second poll and every user
 * mutation both ask for one, and two running at once would read the same
 * outbox, send the same op, and leave the core holding two identical
 * writes. The gate serializes rather than skips: a tap that arrives
 * mid-poll waits its turn and is still sent.
 */
class SyncGate {
    // Activities in the same installation share the native client and outbox.
    // A second activity must not send an operation already being sent elsewhere.
    private companion object { val lock = Mutex() }

    suspend fun <T> serialized(block: suspend () -> T): T = lock.withLock { block() }
}
