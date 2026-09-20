package dev.erisdb.tasks

import org.json.JSONObject

// The write path. Every mutation is committed here before the network is
// touched, and drains in order.
//
// A create names its item with a pending id the moment it is queued, so
// the screen has something to hand back on the next tap. The core mints
// the real id when the create lands, and the alias carries every op that
// still names the pending one — the delete queued behind the create, and
// the edit the user makes a second later from an editor that is still
// holding the pending item.

const val PENDING = "pending-"

/** Aliases matter only while some screen still holds a pending item, so
 * the map keeps the most recent creates and forgets the rest. */
const val ALIAS_LIMIT = 64

/** Where queued ops and the pending-id aliases live. */
interface OutboxStore {
    fun ops(): List<JSONObject>
    fun writeOps(ops: List<JSONObject>)
    fun aliases(): Map<String, String>
    fun writeAliases(aliases: Map<String, String>)
}

/** The id an op really means: its own, or the one the core minted for it. */
fun aliased(aliases: Map<String, String>, id: String): String = aliases[id] ?: id

/** The alias map with `tmp` now pointing at `id`, oldest entries dropped. */
fun remembered(aliases: Map<String, String>, tmp: String, id: String): Map<String, String> {
    val next = LinkedHashMap(aliases)
    next.remove(tmp)
    next[tmp] = id
    while (next.size > ALIAS_LIMIT) next.remove(next.keys.first())
    return next
}

/** What became of one op. */
sealed class Sent {
    /** The transport is down: the op stays queued and ordering holds. */
    data object Retry : Sent()

    /** It landed. `created` is the pending id and the id the core minted. */
    data class Done(val created: Pair<String, String>? = null) : Sent()

    /** Permanently rejected — a schema violation, say. It leaves the queue
     * with a reason rather than retrying forever. */
    data class Dropped(val reason: String) : Sent()
}

/**
 * Why an op is leaving the queue for good.
 *
 * A 403 is the human's answer arriving late: the pairing granted less
 * than this app asked for, and no number of retries turns that into a
 * yes, so the op goes and the permission is named. A 422 naming an
 * unregistered facet is the operator's to fix — this app does not ask for
 * `meta:facets:write`, so it cannot register one itself.
 */
fun rejected(action: String, facet: String, r: JSONObject): Sent.Dropped {
    val status = r.optInt("status")
    val code = r.optJSONObject("body")?.optString("error")
    return Sent.Dropped(
        when {
            status == 403 ->
                "dropped $action: this pairing does not grant $facet:$action"
            status == 422 && code == "unknown_facet" ->
                "dropped $action: the $facet facet is not registered on this core — " +
                    "ask your operator to register it"
            else -> "dropped $action: " + why(r)
        }
    )
}

/** Send one op, following aliases so a pending id never reaches the core. */
suspend fun sendOp(
    api: CoreApi,
    facet: String,
    op: JSONObject,
    aliases: Map<String, String>,
): Sent = when (op.getString("op")) {
    "create" -> {
        val req = JSONObject().put("facet", facet).put("body", op.getJSONObject("body"))
        val r = api.request("POST", "/v1/items", req.toString())
        when {
            r.optInt("status") == 201 ->
                Sent.Done(op.getString("tmp") to r.getJSONObject("body").getString("id"))
            transportDown(r) -> Sent.Retry
            else -> rejected("create", facet, r)
        }
    }

    "update" -> {
        val id = aliased(aliases, op.getString("id"))
        if (id.startsWith(PENDING)) {
            Sent.Dropped("dropped update: its create never landed")
        } else {
            var revision = op.getLong("revision")
            var out: Sent = Sent.Dropped("dropped update: revision conflict persisted")
            for (attempt in 0 until 2) {
                val req = JSONObject()
                    .put("body", op.getJSONObject("body"))
                    .put("revision", revision)
                val r = api.request("PUT", "/v1/items/$id", req.toString())
                if (r.optInt("status") == 200) {
                    out = Sent.Done(); break
                }
                if (transportDown(r)) {
                    out = Sent.Retry; break
                }
                if (r.optInt("status") == 409 && attempt == 0) {
                    // Someone wrote meanwhile — or the item is one this
                    // outbox just created, so the revision we recorded
                    // optimistically is not the one it has. Take the
                    // core's, replay ours on top.
                    val fresh = api.request("GET", "/v1/items/$id")
                    if (fresh.optInt("status") != 200) {
                        out = Sent.Dropped("dropped update: item gone"); break
                    }
                    revision = fresh.getJSONObject("body").getLong("revision")
                    continue
                }
                out = rejected("update", facet, r); break
            }
            out
        }
    }

    "delete" -> {
        val id = aliased(aliases, op.getString("id"))
        if (id.startsWith(PENDING)) {
            // Its create was rejected, so there is nothing on the core to
            // delete and the user's intent is already satisfied.
            Sent.Done()
        } else {
            val r = api.request("DELETE", "/v1/items/$id")
            when {
                r.optInt("status") == 204 || r.optInt("status") == 404 -> Sent.Done()
                transportDown(r) -> Sent.Retry
                else -> rejected("delete", facet, r)
            }
        }
    }

    else -> Sent.Dropped("dropped unknown op")
}

/**
 * Drain in order; stop at the first transport failure so ordering holds.
 * Returns the last drop reason, if any op was rejected permanently.
 *
 * The queue is re-read around every send, so a mutation the user makes
 * while an op is in flight is queued behind it rather than erased by the
 * write-back.
 */
suspend fun drainOutbox(store: OutboxStore, api: CoreApi, facet: String): String? {
    var dropped: String? = null
    while (true) {
        val op = store.ops().firstOrNull() ?: return dropped
        when (val sent = sendOp(api, facet, op, store.aliases())) {
            is Sent.Retry -> return dropped
            is Sent.Dropped -> dropped = sent.reason
            is Sent.Done -> sent.created?.let { (tmp, id) ->
                store.writeAliases(remembered(store.aliases(), tmp, id))
            }
        }
        store.writeOps(store.ops().drop(1))
    }
}

/**
 * The truth the user sees: the core's snapshot with every queued op
 * replayed on top, so a mutation is visible the instant it is enqueued.
 * Ops follow their aliases, so an item whose create has already landed is
 * edited in place rather than appearing twice.
 */
fun applyPending(
    server: Collection<JSONObject>,
    ops: List<JSONObject>,
    aliases: Map<String, String>,
): List<JSONObject> {
    val out = LinkedHashMap<String, JSONObject>()
    for (item in server) out[item.getString("id")] = item
    for (op in ops) when (op.getString("op")) {
        "create" -> {
            val id = aliased(aliases, op.getString("tmp"))
            out[id] = JSONObject()
                .put("id", id)
                .put("body", op.getJSONObject("body"))
                .put("revision", out[id]?.optLong("revision") ?: 0L)
        }
        "update" -> {
            val id = aliased(aliases, op.getString("id"))
            out[id]?.let { out[id] = JSONObject(it.toString()).put("body", op.getJSONObject("body")) }
        }
        "delete" -> out.remove(aliased(aliases, op.getString("id")))
    }
    return out.values.toList()
}
