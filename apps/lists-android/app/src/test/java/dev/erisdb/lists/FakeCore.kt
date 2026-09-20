package dev.erisdb.lists

import kotlinx.coroutines.delay
import org.json.JSONArray
import org.json.JSONObject

/**
 * A core in memory: items with revisions and ids, a change feed, a pairing
 * session and the status codes the app actually turns on. Enough to run
 * the whole read, write and pairing path on the JVM.
 */
class FakeCore(private val facet: String = "lists") : CoreApi {
    val items = LinkedHashMap<String, JSONObject>()
    val changes = mutableListOf<JSONObject>()
    val calls = mutableListOf<String>()

    /** Requests answer `{"status": 0, …}` while this is true. */
    var offline = false

    /** Slows every request, so two callers can overlap on purpose. */
    var latencyMs = 0L

    /** What `GET /v1/permissions` reports for the calling token. */
    var grants: List<String> =
        listOf("lists:read", "lists:create", "lists:update", "lists:delete")

    /** Facet actions this token is refused, as the core refuses them. */
    var forbidden = setOf<String>()

    /** True while the lists facet is registered. When it is not, writes
     * come back 422 unknown_facet the way the real core answers. */
    var facetRegistered = true

    // ------------------------------------------------------------ pairing

    /** False when the bearer is an ordinary capability token rather than
     * a pairing code, so it does not hold `meta:pairing:redeem`. */
    var mayRedeem = true

    /** Past its few minutes: every pairing call is a 400 from here on. */
    var pairExpired = false

    /** Null until a code is redeemed, then one of the session states. */
    var pairStatus: String? = null

    /** Waiting to be handed over, exactly once. */
    var pairToken: String? = null

    var pairGranted: List<String> = emptyList()

    /** Every redeem request body this core was sent. */
    val redeemed = mutableListOf<JSONObject>()

    /** The operator answering yes, with what they actually granted. */
    fun approve(granted: List<String>, token: String) {
        pairStatus = "approved"
        pairGranted = granted
        pairToken = token
        grants = granted
    }

    /** The operator answering no. */
    fun deny() {
        pairStatus = "denied"
    }

    private var nextId = 0
    private var seq = 0L
    private var clock = 0L

    override suspend fun request(method: String, path: String, body: String?): JSONObject {
        calls += "$method $path"
        if (latencyMs > 0) delay(latencyMs)
        if (offline) return JSONObject().put("status", 0).put("error", "transport down")
        return route(method, path, body)
    }

    /** Every request the fake saw, as `"METHOD /path"`. */
    fun callsMatching(prefix: String): List<String> = calls.filter { it.startsWith(prefix) }

    // ------------------------------------------------------------ routing

    private fun route(method: String, path: String, body: String?): JSONObject = when {
        method == "GET" && path == "/v1/permissions" -> permissions()
        method == "POST" && path == "/v1/pair/redeem" -> redeem(JSONObject(body!!))
        method == "GET" && path == "/v1/pair/status" -> pairStatusOf()
        method == "POST" && path == "/v1/items" -> create(JSONObject(body!!))
        method == "GET" && path.startsWith("/v1/items?") -> list(query(path))
        method == "GET" && path.startsWith("/v1/changes?") -> feed(query(path))
        method == "GET" && path.startsWith("/v1/items/") -> read(path.substringAfterLast('/'))
        method == "PUT" && path.startsWith("/v1/items/") ->
            update(path.substringAfterLast('/'), JSONObject(body!!))
        method == "DELETE" && path.startsWith("/v1/items/") -> delete(path.substringAfterLast('/'))
        else -> answer(404)
    }

    private fun permissions(): JSONObject = answer(
        200,
        JSONObject()
            .put("grants", JSONArray(grants))
            .put("exp", JSONObject.NULL)
            .put("max_exp", JSONObject.NULL)
            .put("user", JSONObject.NULL),
    )

    private fun redeem(req: JSONObject): JSONObject {
        if (!mayRedeem) {
            return refuse(403, "forbidden", "capability does not grant meta:pairing:redeem")
        }
        if (pairExpired) return refuse(400, "bad_request", "this pairing code has expired")
        redeemed += req
        if (pairStatus != null) {
            return refuse(409, "conflict", "this pairing code is already $pairStatus")
        }
        pairStatus = "requested"
        // The core answers with the session as an onlooker may see it.
        return answer(200, JSONObject().put("id", "pair-0").put("revision", 2L))
    }

    private fun pairStatusOf(): JSONObject {
        if (!mayRedeem) {
            return refuse(403, "forbidden", "capability does not grant meta:pairing:redeem")
        }
        if (pairExpired) return refuse(400, "bad_request", "this pairing code has expired")
        val body = JSONObject()
            .put("status", if (pairStatus == "collected") "approved" else pairStatus ?: "pending")
            .put("granted", JSONArray(pairGranted))
        // Unit fixture models repeatable collection by this same app.
        // Real identity binding is exercised against PostgreSQL/Iroh in E2E.
        pairToken?.let {
            body.put("token", it)
            pairStatus = "collected"
        }
        return answer(200, body)
    }

    private fun create(req: JSONObject): JSONObject {
        if ("create" in forbidden) {
            return refuse(403, "forbidden", "capability does not grant $facet:create")
        }
        if (!facetRegistered) {
            return refuse(422, "unknown_facet", "facet $facet is not registered")
        }
        val id = "item-${nextId++}"
        val item = JSONObject()
            .put("id", id)
            .put("facet", req.getString("facet"))
            .put("body", req.getJSONObject("body"))
            .put("revision", 1L)
            .put("created_at", stamp())
            .put("updated_at", stamp())
        items[id] = item
        record(id, "created", item)
        return answer(201, item)
    }

    private fun read(id: String): JSONObject =
        items[id]?.let { answer(200, it) } ?: answer(404)

    private fun update(id: String, req: JSONObject): JSONObject {
        if ("update" in forbidden) {
            return refuse(403, "forbidden", "capability does not grant $facet:update")
        }
        val item = items[id] ?: return answer(404)
        if (item.getLong("revision") != req.getLong("revision")) return answer(409)
        val next = JSONObject(item.toString())
            .put("body", req.getJSONObject("body"))
            .put("revision", item.getLong("revision") + 1)
            .put("updated_at", stamp())
        items[id] = next
        record(id, "updated", next)
        return answer(200, next)
    }

    private fun delete(id: String): JSONObject {
        if ("delete" in forbidden) {
            return refuse(403, "forbidden", "capability does not grant $facet:delete")
        }
        if (items.remove(id) == null) return answer(404)
        record(id, "deleted", null)
        return answer(204)
    }

    private fun list(q: Map<String, String>): JSONObject {
        val limit = q["limit"]?.toInt() ?: 100
        val since = q["updated_since"]
        val page = items.values
            .filter { it.getString("facet") == facet }
            .sortedBy { it.getString("updated_at") }
            .filter { since == null || it.getString("updated_at") > since }
            .take(limit)
        return answer(200, JSONObject().put("items", JSONArray(page)))
    }

    private fun feed(q: Map<String, String>): JSONObject {
        val since = q["since"]?.toLong() ?: 0L
        val limit = q["limit"]?.toInt() ?: 500
        val page = changes.filter { it.getLong("seq") > since }.take(limit)
        val next = page.lastOrNull()?.getLong("seq") ?: since
        return answer(200, JSONObject().put("changes", JSONArray(page)).put("next", next))
    }

    // ------------------------------------------------------------ helpers

    private fun record(id: String, op: String, item: JSONObject?) {
        changes += JSONObject()
            .put("seq", ++seq)
            .put("item_id", id)
            .put("facet", facet)
            .put("op", op)
            .put("at", stamp())
            .put("body", item?.getJSONObject("body") ?: JSONObject.NULL)
            .put("revision", item?.getLong("revision") ?: JSONObject.NULL)
    }

    /** Fixed width and strictly increasing, so `updated_since` paging can
     * compare two of these as strings the way the core compares instants. */
    private fun stamp(): String = "2026-01-01T00:00:00.%09dZ".format(clock++)

    private fun answer(status: Int, body: JSONObject? = null): JSONObject =
        JSONObject().put("status", status).also { if (body != null) it.put("body", body) }

    /** A refusal shaped the way the core's error envelope is shaped, so
     * `why()` reads the same detail here as it does on a phone. */
    private fun refuse(status: Int, code: String, detail: String): JSONObject =
        answer(status, JSONObject().put("error", code).put("detail", detail))

    private fun query(path: String): Map<String, String> =
        path.substringAfter('?').split('&').associate {
            val (k, v) = it.split('=', limit = 2)
            k to java.net.URLDecoder.decode(v, "UTF-8")
        }
}

/** An outbox in memory, with the same contract as the on-disk one. */
class FakeOutbox : OutboxStore {
    private var queue = listOf<JSONObject>()
    private var alias = mapOf<String, String>()

    override fun ops(): List<JSONObject> = queue
    override fun writeOps(ops: List<JSONObject>) { queue = ops }
    override fun aliases(): Map<String, String> = alias
    override fun writeAliases(aliases: Map<String, String>) { alias = aliases }

    fun enqueue(op: JSONObject) { queue = queue + op }
}

fun createOp(tmp: String, body: String): JSONObject = JSONObject()
    .put("op", "create").put("tmp", tmp).put("body", JSONObject(body))

fun updateOp(id: String, revision: Long, body: String): JSONObject = JSONObject()
    .put("op", "update").put("id", id).put("revision", revision).put("body", JSONObject(body))

fun deleteOp(id: String): JSONObject = JSONObject().put("op", "delete").put("id", id)
