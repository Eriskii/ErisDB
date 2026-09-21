package dev.erisdb.lists

import org.json.JSONObject

// The read path: a snapshot once, then the change feed forever.
//
// `GET /v1/changes?since=<seq>` returns every write the facet has taken
// past a sequence number the client holds, and each row carries the whole
// body the write produced. So the cache is seeded once and kept true by
// deltas — the whole store crosses the network on first contact and never
// again, and a store larger than one page is no longer silently cut off
// at the thousandth item.

/** `list_items` clamps `limit` to 1000. */
const val ITEM_PAGE = 1000

/** `list_changes` clamps `limit` to 5000; 500 is a comfortable poll. */
const val FEED_PAGE = 500

/**
 * Every item in the facet, however many there are. `list_items` orders by
 * `updated_at`, so pages are walked forward with `updated_since`. A page
 * that adds no id ends the walk — the cursor cannot step past a tie, and
 * stopping is better than spinning.
 */
suspend fun fetchAllItems(api: CoreApi, facet: String): Read<Map<String, JSONObject>> {
    val byId = LinkedHashMap<String, JSONObject>()
    var since: String? = null
    while (true) {
        val path = StringBuilder("/v1/items?facet=${enc(facet)}&limit=$ITEM_PAGE")
        if (since != null) path.append("&updated_since=").append(enc(since))
        val r = api.request("GET", path.toString())
        if (r.optInt("status") != 200) return Read.Failed(why(r))
        val page = jsonObjects(r.getJSONObject("body").getJSONArray("items"))
        val fresh = page.count { byId.put(it.getString("id"), it) == null }
        if (page.size < ITEM_PAGE || fresh == 0) return Read.Ok(byId)
        since = page.last().getString("updated_at")
    }
}

/**
 * The feed's head sequence. `list_changes` answers at most one page and
 * reports the last seq it saw, so the head is reached by walking.
 */
suspend fun feedHead(api: CoreApi, facet: String): Read<Long> {
    var cursor = 0L
    while (true) {
        val r = api.request("GET", "/v1/changes?since=$cursor&facet=${enc(facet)}&limit=5000")
        if (r.optInt("status") != 200) return Read.Failed(why(r))
        val body = r.getJSONObject("body")
        val next = body.optLong("next", cursor)
        if (body.getJSONArray("changes").length() == 0 || next <= cursor) return Read.Ok(cursor)
        cursor = next
    }
}

/**
 * One change row folded into the cache. Rows carry the whole snapshot the
 * write produced, so the feed alone keeps the cache true — no per-item
 * refetch. A `lapsed` row is the core noticing a due date has passed
 * rather than a write, so it leaves the item's timestamps where they are.
 */
fun applyChange(cache: MutableMap<String, JSONObject>, ch: JSONObject) {
    val id = ch.optString("item_id").ifEmpty { return }
    if (ch.optString("op") == "deleted") {
        cache.remove(id)
        return
    }
    val body = ch.optJSONObject("body") ?: return
    val known = cache[id]
    if (ch.optString("op") == "lapsed" && known != null) {
        cache[id] = JSONObject(known.toString())
            .put("body", body)
            .put("revision", ch.optLong("revision", known.optLong("revision")))
        return
    }
    cache[id] = JSONObject()
        .put("id", id)
        .put("facet", ch.optString("facet"))
        .put("body", body)
        .put("revision", ch.optLong("revision"))
        .put("created_at", known?.optString("created_at")?.ifEmpty { null } ?: ch.optString("at"))
        .put("updated_at", ch.optString("at"))
}

/**
 * Everything the feed holds past `cursor`, folded into `cache`, page by
 * page until it is caught up. Returns the cursor to hold; a failure
 * leaves both the cache and the caller's cursor untouched from here on,
 * so the next poll simply asks again.
 */
suspend fun pollChanges(
    api: CoreApi,
    facet: String,
    cursor: Long,
    cache: MutableMap<String, JSONObject>,
): Read<Long> {
    var at = cursor
    while (true) {
        val r = api.request("GET", "/v1/changes?since=$at&facet=${enc(facet)}&limit=$FEED_PAGE")
        if (r.optInt("status") != 200) return Read.Failed(why(r))
        val body = r.getJSONObject("body")
        val changes = jsonObjects(body.getJSONArray("changes"))
        for (ch in changes) applyChange(cache, ch)
        val before = at
        at = maxOf(at, body.optLong("next", at))
        if (changes.size < FEED_PAGE || at <= before) return Read.Ok(at)
    }
}

/** The cache and the feed position that go together. */
data class Snapshot(val items: Map<String, JSONObject>, val cursor: Long)

/**
 * First contact: read the feed's head, then the items. In that order —
 * the snapshot is then at least as new as the cursor, and the changes
 * that landed in between simply replay on the next poll. Replaying is
 * free: a change row is the state the write produced, so applying one
 * twice lands in the same place.
 *
 */
suspend fun seed(api: CoreApi, facet: String): Read<Snapshot> {
    val head = when (val h = feedHead(api, facet)) {
        is Read.Failed -> return h
        is Read.Ok -> h.value
    }
    return when (val items = fetchAllItems(api, facet)) {
        is Read.Failed -> items
        is Read.Ok -> Read.Ok(Snapshot(items.value, head))
    }
}
