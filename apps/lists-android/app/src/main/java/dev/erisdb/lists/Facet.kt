package dev.erisdb.lists

import org.json.JSONObject

// A facet's name is also its permission namespace, so the schema version
// lives in the body: the grant `lists:read` survives a move to schema v2,
// which is what a permission should do.
//
// Registering one needs `meta:facets:write` — register, change and remove
// *every* facet on the core — which this app does not ask for (see
// MANIFEST). So this runs only for an operator who granted `*` anyway,
// and an unregistered facet reads as "ask your operator" on the first
// write instead.

private const val SCHEMA = """{"type":"object","required":["list","name"],"properties":{
    "list":{"type":"string","minLength":1},
    "name":{"type":"string","minLength":1},
    "description":{"type":"string"},
    "link":{"type":"string"},
    "attributes":{"type":"object","additionalProperties":{"anyOf":[
        {"type":["string","number","boolean","null"]},{"type":"array"}]}}
},"additionalProperties":false}"""

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
    .put("permissions", descriptions())

/**
 * Register the facet — for a token that holds `meta:facets:write`, which
 * only an operator who granted `*` or `meta:*` has handed over. Everyone
 * else skips it, and the facet is the operator's to register. 409 means
 * it is already there, which is the ordinary case.
 */
suspend fun ensureFacet(api: CoreApi, grants: Grants) {
    if (!grants.can("meta:facets:write")) return
    val req = JSONObject().put("facet", "facet").put("body", facetBody())
    api.request("POST", "/v1/items", req.toString())
}
