package dev.erisdb.tasks

import android.annotation.SuppressLint
import android.content.Context
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
//   settings  — server id, token lifetime, feed cursor: a handful of
//               small values, which is what SharedPreferences is for;
//   the cache — one file. SharedPreferences reads its whole XML into
//               memory and rewrites all of it on every commit, which is
//               the wrong shape for hundreds of kilobytes of items.

@SuppressLint("ApplySharedPref")
class Store(ctx: Context) : OutboxStore {
    private val prefs = ctx.getSharedPreferences("erisdb", Context.MODE_PRIVATE)
    private val secrets: SecretStore = KeystoreSecrets(ctx)
    private val cacheFile = File(ctx.filesDir, "items.json")

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

    /** The feed sequence this device has read up to, or null before first
     * contact. Held here, so a reinstall reseeds and a restart does not. */
    var cursor: Long?
        get() = if (prefs.contains("cursor")) prefs.getLong("cursor", 0L) else null
        set(value) {
            val edit = prefs.edit()
            if (value == null) edit.remove("cursor") else edit.putLong("cursor", value)
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

    fun hasCache(): Boolean = cacheFile.exists()

    fun loadItems(): MutableMap<String, JSONObject> {
        val out = LinkedHashMap<String, JSONObject>()
        if (!cacheFile.exists()) return out
        runCatching {
            val arr = JSONArray(cacheFile.readText())
            for (item in jsonObjects(arr)) out[item.getString("id")] = item
        }
        return out
    }

    fun saveItems(items: Map<String, JSONObject>) {
        val tmp = File(cacheFile.parentFile, cacheFile.name + ".tmp")
        tmp.writeText(JSONArray(items.values.toList()).toString())
        tmp.renameTo(cacheFile)
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
