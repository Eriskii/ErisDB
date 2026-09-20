package dev.erisdb.lists

import android.util.Base64
import org.json.JSONObject

// Tokens carry their own expiry. The app refreshes at half-life via
// POST /v1/capabilities/refresh, always asking for the lifetime the admin
// chose at mint time — refresh moves time, not privilege.

/** The token's exp claim (unix seconds), or null when it carries none
 * (or the string isn't an ErisDB token — read the same way: no expiry). */
fun tokenExp(token: String): Long? = try {
    val payload = token.split(".").getOrNull(1)
    val json = JSONObject(String(Base64.decode(payload, Base64.URL_SAFE)))
    if (json.isNull("exp")) null else json.getLong("exp")
} catch (_: Exception) {
    null
}

/** What a pasted token is worth. */
sealed class Lifetime {
    /** No exp claim: it stands until revoked, and is never refreshed. */
    data object Permanent : Lifetime()

    /** exp − now: the lifetime every refresh preserves. */
    data class Seconds(val value: Long) : Lifetime()

    /** exp is not in the future. The core rejects this token on sight, and
     * no refresh can rescue it — only a freshly minted one will do. */
    data object Dead : Lifetime()
}

/** Read a token's lifetime at the moment it is pasted. A non-positive
 * remainder is not a short lifetime, it is a dead token, and saying so is
 * the only useful thing to do with it. */
fun lifetimeOf(exp: Long?, nowSec: Long): Lifetime = when {
    exp == null -> Lifetime.Permanent
    exp > nowSec -> Lifetime.Seconds(exp - nowSec)
    else -> Lifetime.Dead
}

/**
 * True when the core turned a refresh down because the capability is no
 * longer good, as opposed to the network being unreachable.
 *
 * erisdb-client reports failures as free text over the JNI boundary, so a
 * numeric `status` is preferred wherever the envelope carries one and the
 * text is only read as a fallback.
 */
fun refreshRejected(r: JSONObject): Boolean {
    if (r.has("status") && !r.isNull("status")) return r.optInt("status") == 401
    val error = r.optString("error").lowercase()
    return "401" in error || "unauthorized" in error || "expired" in error
}
