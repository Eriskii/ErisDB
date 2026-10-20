package dev.erisdb.lists

import dev.erisdb.android.*

import android.annotation.SuppressLint
import android.content.Context
import android.util.AtomicFile
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

// Offline-first: the item cache renders instantly on launch, and every
// mutation is committed to the on-disk outbox BEFORE any network is
// attempted — killing the app mid-write loses nothing; the op replays on
// next launch.
//
// Three kinds of thing live here, in three places that suit them:
//
//   secrets   — the app's Iroh key and the ErisDB it was let into, sealed
//               together by a keystore key (android-shared's Secrets.kt);
//   settings  — a handful of small values, which is what SharedPreferences
//               is for;
//   the cache — one file. SharedPreferences reads its whole XML into
//               memory and rewrites all of it on every commit, which is
//               the wrong shape for hundreds of kilobytes of items.

@SuppressLint("ApplySharedPref")
class Store(ctx: Context) : OutboxStore {
    private val prefs = ctx.getSharedPreferences("erisdb", Context.MODE_PRIVATE)
    private val cacheFile = AtomicFile(File(ctx.filesDir, "items.json"))

    /** The app's key and where its ErisDB is. */
    val credentials = Credentials(KeystoreSecrets(ctx, "erisdb.lists.secrets"))

    // ------------------------------------------------------------ settings

    /** Two-column mosaic vs. single-column list on the main screen. */
    var grid: Boolean
        get() = prefs.getBoolean("grid", false)
        set(value) {
            prefs.edit().putBoolean("grid", value).commit()
        }

    /** Whether an entry's link may be fetched for a preview image. Off
     * until asked for: see LinkPreview.kt. */
    var linkPreviews: Boolean
        get() = prefs.getBoolean("link_previews", false)
        set(value) {
            prefs.edit().putBoolean("link_previews", value).commit()
        }

    /**
     * What ErisDB last reported this app holds, or null before it has been
     * asked. Kept in the clear: it is what the person approved, and ErisDB
     * is the one that enforces it — this copy only decides which buttons
     * are worth drawing.
     */
    var grants: List<String>?
        get() = readGrants(prefs.getString("grants", null))
        set(value) {
            val edit = prefs.edit()
            val stored = writeGrants(value)
            if (stored == null) edit.remove("grants") else edit.putString("grants", stored)
            edit.commit()
        }

    // ------------------------------------------------------------ cache

    /** Items and the position that produced them are one snapshot, kept with
     * the endpoint id of the ErisDB they came from. An absent, unreadable, or
     * other ErisDB's snapshot must be fetched again in full. */
    fun loadSnapshot(): Snapshot? = runCatching {
        val saved = JSONObject(String(cacheFile.readFully(), Charsets.UTF_8))
        require(saved.getString("server") == credentials.server?.let(::serverId))
        val cursor = saved.getLong("cursor")
        require(cursor >= 0)
        val items = LinkedHashMap<String, JSONObject>()
        for (item in jsonObjects(saved.getJSONArray("items"))) {
            item.getJSONObject("body")
            items[item.getString("id")] = item
        }
        Snapshot(items, cursor)
    }.getOrNull()

    /** AtomicFile syncs and replaces the whole snapshot. No separately saved
     * cursor can move past items that failed to reach disk. */
    fun saveSnapshot(server: String, snapshot: Snapshot) {
        val bytes = JSONObject().put("server", serverId(server)).put("cursor", snapshot.cursor)
            .put("items", JSONArray(snapshot.items.values.toList())).toString().toByteArray(Charsets.UTF_8)
        val stream = cacheFile.startWrite()
        try {
            stream.write(bytes)
            cacheFile.finishWrite(stream)
        } catch (error: Exception) {
            cacheFile.failWrite(stream)
            throw error
        }
    }

    // ------------------------------------------------------------ outbox

    override fun ops(): List<JSONObject> = parseArray(prefs.getString("outbox", null))

    /** Durable BEFORE returning: commit(), not apply(). This is the moment
     * a mutation becomes kill-proof. */
    override fun writeOps(ops: List<JSONObject>) {
        prefs.edit().putString("outbox", JSONArray(ops).toString()).commit()
    }

    override fun aliases(): Map<String, String> {
        val out = LinkedHashMap<String, String>()
        runCatching {
            val obj = JSONObject(prefs.getString("aliases", null) ?: "{}")
            for (key in obj.keys()) out[key] = obj.getString(key)
        }
        return out
    }

    override fun writeAliases(aliases: Map<String, String>) {
        val obj = JSONObject()
        for ((tmp, id) in aliases) obj.put(tmp, id)
        prefs.edit().putString("aliases", obj.toString()).commit()
    }

    fun enqueue(op: JSONObject) = writeOps(ops() + op)

    private fun parseArray(s: String?): List<JSONObject> =
        runCatching { jsonObjects(JSONArray(s ?: "[]")) }.getOrDefault(emptyList())
}
