package dev.erisdb.tasks

import android.annotation.SuppressLint
import android.content.Context
import android.util.AtomicFile
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.security.SecureRandom

// Offline-first: the item cache renders instantly on launch, and every
// mutation is committed to the on-disk outbox BEFORE any network is
// attempted — killing the app mid-write loses nothing; the op replays on
// next launch.
//
// Three kinds of thing live here, in three places that suit them:
//
//   secrets   — the token and the iroh private key, sealed by a keystore
//               key (Secrets.kt), never in the clear;
//   settings  — server id and token lifetime: a handful of
//               small values, which is what SharedPreferences is for;
//   the cache — one file. SharedPreferences reads its whole XML into
//               memory and rewrites all of it on every commit, which is
//               the wrong shape for hundreds of kilobytes of items.

@SuppressLint("ApplySharedPref")
class Store(ctx: Context) : OutboxStore {
    private val prefs = ctx.getSharedPreferences("erisdb", Context.MODE_PRIVATE)
    private val secrets: SecretStore = KeystoreSecrets(ctx)
    private val cacheFile = AtomicFile(File(ctx.filesDir, "items.json"))

    // ------------------------------------------------------------ settings

    var server: String
        get() = prefs.getString("server", "") ?: ""
        set(value) {
            prefs.edit().putString("server", value).commit()
        }

    /** The label a pairing ticket gave this core, so the app can say
     * *paired with my-laptop*. Never trusted for anything. */
    var coreName: String
        get() = prefs.getString("coreName", "") ?: ""
        set(value) {
            prefs.edit().putString("coreName", value).commit()
        }

    /** The token lifetime observed at connect (exp − now), the lifetime
     * every refresh preserves; 0 means the token never expires. */
    var ttl: Long
        get() = prefs.getLong("ttl", 0L)
        set(value) {
            prefs.edit().putLong("ttl", value).commit()
        }

    /**
     * What the core last reported this token holds, or null before it has
     * been asked. Kept in the clear: it is what an operator approved, and
     * the core is the one that enforces it — this copy only decides which
     * buttons are worth drawing.
     */
    var grants: List<String>?
        get() = readGrants(prefs.getString("grants", null))
        set(value) {
            val edit = prefs.edit()
            val stored = writeGrants(value)
            if (stored == null) edit.remove("grants") else edit.putString("grants", stored)
            edit.commit()
        }

    // ------------------------------------------------------------ secrets

    /** Durable before returning: a kill right after a refresh must not
     * lose the fresh token and leave the app holding a dead one. */
    var token: String
        get() = secrets.get("token") ?: ""
        set(value) = secrets.put("token", value)

    /** The device's iroh identity: 32 random bytes, minted once and kept
     * forever, so `source.addr` names this phone stably. */
    fun identityHex(): String {
        secrets.get("identity")?.let { return it }
        val bytes = ByteArray(32).also { SecureRandom().nextBytes(it) }
        val hex = bytes.joinToString("") { "%02x".format(it) }
        secrets.put("identity", hex)
        return hex
    }

    // ------------------------------------------------------------ cache

    /** Items and the position that produced them are one snapshot. An absent,
     * unreadable, or different-core snapshot must be fetched again in full. */
    fun loadSnapshot(): Snapshot? = runCatching {
        val saved = JSONObject(String(cacheFile.readFully(), Charsets.UTF_8))
        require(saved.getString("server") == server)
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
        val bytes = JSONObject().put("server", server).put("cursor", snapshot.cursor)
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

    // ------------------------------------------------------------ notices

    /** Item id → the revision already announced, so a task is announced
     * once per edit rather than once per poll. */
    fun announced(): Map<String, Long> {
        val out = LinkedHashMap<String, Long>()
        runCatching {
            val obj = JSONObject(prefs.getString("announced", null) ?: "{}")
            for (key in obj.keys()) out[key] = obj.getLong(key)
        }
        return out
    }

    fun writeAnnounced(announced: Map<String, Long>) {
        val obj = JSONObject()
        for ((id, revision) in announced) obj.put(id, revision)
        prefs.edit().putString("announced", obj.toString()).commit()
    }

    private fun parseArray(s: String?): List<JSONObject> =
        runCatching { jsonObjects(JSONArray(s ?: "[]")) }.getOrDefault(emptyList())
}
