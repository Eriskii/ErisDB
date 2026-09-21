package dev.erisdb.lists

import org.json.JSONObject

// A facet's name is also its permission namespace, so the schema version
// lives in the body: the grant `lists:read` survives a move to schema v2,
// which is what a permission should do.
//
// `lists:create` can initialize this namespace when absent. It cannot
// replace a schema or administer other namespaces.

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

/** Initialize this namespace before saving; 409 leaves the existing schema intact.
 * Return an error so a failed setup keeps pending entries queued. */
suspend fun ensureFacet(api: CoreApi, grants: Grants): String? {
    if (!grants.can("$FACET:create") && !grants.can("meta:facets:write")) return null
    val req = JSONObject().put("facet", "facet").put("body", facetBody())
    val response = api.request("POST", "/v1/items", req.toString())
    return if (response.optInt("status") in listOf(201, 409)) null else why(response)
}
