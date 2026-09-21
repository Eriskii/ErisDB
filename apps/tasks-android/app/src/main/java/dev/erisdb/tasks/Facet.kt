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

/** Register the schema when absent and explicitly authorized. */
suspend fun ensureFacet(api: CoreApi, grants: Grants) {
    if (!grants.can("meta:facets:write")) return
    val req = JSONObject().put("facet", "facet").put("body", facetBody())
    api.request("POST", "/v1/items", req.toString())
}
