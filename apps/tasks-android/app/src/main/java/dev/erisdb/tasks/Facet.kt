package dev.erisdb.tasks

import org.json.JSONObject

// The tasks facet is shared with the web client. A facet's name is also
// its permission namespace, so the schema version lives in the body: the
// grant `tasks:read` survives a move to schema v2, which is what a
// permission should do.
//
// Registering one needs `meta:facets:write` — register, change and remove
// *every* facet on the core — which this app does not ask for (see
// MANIFEST). So this runs only for an operator who granted `*` anyway,
// and an unregistered facet reads as "ask your operator" on the first
// write instead.

private const val SCHEMA = """{"type":"object","required":["title","done"],"properties":{
    "title":{"type":"string","minLength":1},
    "done":{"type":"boolean"},
    "due":{"type":"string","format":"date-time"},
    "notes":{"type":"string"},
    "repeat":{"type":"object","required":["n","unit"],"properties":{
        "n":{"type":"integer","minimum":1},
        "unit":{"enum":["day","week","month","year"]}},"additionalProperties":false}
},"additionalProperties":false}"""

/** Lapse tells the core which fields make a task overdue, so it emits a
 * `lapsed` change when a due date passes with `done` still false. */
private const val LAPSE = """{"due":"due","done":"done"}"""

/** The schema this app writes to. It moves with the schema, not with the
 * facet name, so grants outlive it. */
const val FACET_VERSION = 1

/** Human descriptions, so an approval prompt reads in sentences rather
 * than permission strings. Presentation only: no grant depends on them. */
private fun descriptions(): JSONObject = JSONObject()
    .put("read", describeGrant("$FACET:read"))
    .put("create", describeGrant("$FACET:create"))
    .put("update", describeGrant("$FACET:update"))
    .put("delete", describeGrant("$FACET:delete"))

private fun facetBody(): JSONObject = JSONObject()
    .put("name", FACET)
    .put("version", FACET_VERSION)
    .put("strict", true)
    .put("schema", JSONObject(SCHEMA))
    .put("lapse", JSONObject(LAPSE))
    .put("permissions", descriptions())

/**
 * Register the facet, or bring an older registration up to this schema —
 * for a token that holds `meta:facets:write`, which only an operator who
 * granted `*` or `meta:*` has handed over. Everyone else skips it, and the
 * facet is the operator's to register.
 */
suspend fun ensureFacet(api: CoreApi, grants: Grants) {
    if (!grants.can("meta:facets:write")) return
    val req = JSONObject().put("facet", "facet").put("body", facetBody())
    val r = api.request("POST", "/v1/items", req.toString())
    // Reading a registration back is a separate permission from writing
    // one, so an upgrade needs both.
    if (r.optInt("status") == 409 && grants.can("meta:facets:read")) upgradeFacet(api)
}

private suspend fun upgradeFacet(api: CoreApi) {
    val list = api.request("GET", "/v1/items?facet=facet&limit=$ITEM_PAGE")
    if (list.optInt("status") != 200) return
    val registered = jsonObjects(list.getJSONObject("body").getJSONArray("items"))
        .firstOrNull { it.getJSONObject("body").optString("name") == FACET } ?: return
    val body = registered.getJSONObject("body")
    val properties = body.optJSONObject("schema")?.optJSONObject("properties")
    // Already this schema, and already carrying the version and the
    // descriptions a pairing prompt reads out.
    if (properties != null && properties.has("repeat") &&
        body.optInt("version") == FACET_VERSION && body.has("permissions")
    ) return

    val id = registered.getString("id")
    var revision = registered.getLong("revision")
    for (attempt in 0 until 2) {
        val req = JSONObject().put("body", facetBody()).put("revision", revision)
        val r = api.request("PUT", "/v1/items/$id", req.toString())
        if (r.optInt("status") == 200) return
        if (r.optInt("status") == 409 && attempt == 0) {
            val fresh = api.request("GET", "/v1/items/$id")
            if (fresh.optInt("status") != 200) return
            revision = fresh.getJSONObject("body").getLong("revision")
            continue
        }
        return
    }
}
