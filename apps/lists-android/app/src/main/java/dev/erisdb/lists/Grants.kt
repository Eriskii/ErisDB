package dev.erisdb.lists

import org.json.JSONArray

// What this app may do is decided by a person, not by this app.
//
// Pairing asks for a set of grants and a human answers — with all of it,
// some of it, or no. So "what I asked for" and "what I hold" are two
// different things, and every button that writes goes through `can`.
//
// The grammar is docs/permissions.md's, and `covers` below is the same
// function the core runs. That is the point: a button this app shows is a
// request the core will take, and a button it hides is one the core would
// refuse. Anything else is a 403 the user did not deserve.

/**
 * The grants this app asks for when it redeems a pairing code.
 *
 * Asking is free and the answer is a person, so the list is exactly what
 * the app uses and nothing wider. `create` and `update` are separate
 * permissions, so an operator can hand out "add entries but do not touch
 * mine" and this app will honour it.
 *
 * Creating items also permits initializing this app's missing schema.
 * Global facet administration is not requested; existing schemas stay intact.
 */
val MANIFEST = listOf(
    "$FACET:read",    // the entries themselves, their history, and the change feed
    "$FACET:create",  // add an entry
    "$FACET:update",  // edit one
    "$FACET:delete",  // delete one
)

/**
 * True when `grant` covers `required`, segment by segment.
 *
 * `*` matches exactly one segment, and as a grant's **final** segment it
 * matches every remaining segment — which is what makes `*` mean
 * everything and `meta:*` reach `meta:pairing:approve`, while `*:read`
 * ends in a literal and so stops at two segments. Read-everything and
 * administer-everything are different grants and neither implies the
 * other.
 *
 * The same function decides whether one grant subsumes another: pass the
 * child as `required`. `*` is wild on the left and an ordinary token on
 * the right, so `lists:read` does not cover `lists:*`.
 */
fun covers(grant: String, required: String): Boolean {
    val g = grant.split(':')
    val r = required.split(':')
    for ((i, seg) in g.withIndex()) {
        // Trailing wildcard: the rest, and there must be a rest.
        if (seg == "*" && i == g.size - 1) return r.size >= g.size
        val other = r.getOrNull(i) ?: return false
        if (seg != "*" && seg != other) return false
    }
    return r.size == g.size
}

/**
 * The grants a token is known to hold.
 *
 * `held` is null until `GET /v1/permissions` has answered once. Unknown
 * is not the same as none: an app that has never managed to ask shows its
 * whole self and lets a 403 correct it, rather than greying out the Add
 * button because the network was down at launch.
 */
data class Grants(val held: List<String>?) {
    fun can(required: String): Boolean = held?.any { covers(it, required) } ?: true

    val mayRead: Boolean get() = can("$FACET:read")
    val mayCreate: Boolean get() = can("$FACET:create")
    val mayUpdate: Boolean get() = can("$FACET:update")
    val mayDelete: Boolean get() = can("$FACET:delete")

    /** Read granted and nothing else. The app shows every entry and
     * refuses every change, which is a state worth naming on screen. */
    val readOnly: Boolean
        get() = held != null && mayRead && !mayCreate && !mayUpdate && !mayDelete

    companion object {
        /** Before the core has been asked. */
        val UNKNOWN = Grants(null)
    }
}

/** What a grant means in a sentence, for the screens that name one. A
 * permission with nothing to say shows itself. */
fun describeGrant(grant: String): String = when (grant) {
    "$FACET:read" -> "read your lists"
    "$FACET:create" -> "add entries"
    "$FACET:update" -> "change entries"
    "$FACET:delete" -> "delete entries"
    else -> grant
}

/** The grants asked for that were not given, so the app can say what it
 * cannot do rather than only going quiet. */
fun withheld(grants: Grants, asked: List<String>): List<String> =
    if (grants.held == null) emptyList() else asked.filterNot { grants.can(it) }

/** Grants as they are kept between launches. Null stays null: never
 * asked and granted nothing are different answers. */
fun writeGrants(held: List<String>?): String? = held?.let { JSONArray(it).toString() }

fun readGrants(stored: String?): List<String>? = stored?.let {
    runCatching {
        val arr = JSONArray(it)
        (0 until arr.length()).map { i -> arr.getString(i) }
    }.getOrNull()
}

/**
 * `GET /v1/permissions`: what this token actually holds. It requires no
 * permission of its own, so the answer is always either the truth or a
 * transport failure — and a transport failure leaves the last known set
 * in place rather than pretending the token lost its grants.
 */
suspend fun fetchGrants(api: CoreApi): Read<Grants> {
    val r = api.request("GET", "/v1/permissions")
    if (r.optInt("status") != 200) return Read.Failed(why(r), r.optInt("status"))
    val arr = r.getJSONObject("body").optJSONArray("grants") ?: JSONArray()
    return Read.Ok(Grants((0 until arr.length()).map { arr.getString(it) }))
}
