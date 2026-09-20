package dev.erisdb.tasks

import org.json.JSONArray
import org.json.JSONObject

// A ticket carries a pairing code, not a capability. The code grants
// exactly `meta:pairing:redeem` on one session and is good for minutes,
// so a photographed QR gets an attacker as far as raising a prompt on
// someone else's screen — naming a client they did not install, asking
// for permissions they can refuse.
//
// Turning a code into a token takes two steps and a person:
//
//   POST /v1/pair/redeem   — who this app is, and what it would like
//   GET  /v1/pair/status   — polled until a human answers
//
// The token comes back exactly once: collecting it clears it from the
// session, so a replayed code cannot fetch it again. That is why `collect`
// hands it to its caller before returning, and why the caller's job is to
// put it on disk and nothing else.

/** How long between polls while a human is deciding. */
const val PAIR_POLL_MS = 1500L

/** Where a pairing session stands, as this app sees it. */
sealed class Approval {
    /** Redeemed. Somebody has to press a key on another machine. */
    data class Waiting(val fingerprint: String? = null) : Approval()

    /** Answered yes. `token` has already been handed to the keeper, and
     * `granted` is what the human actually approved — which may be less
     * than the manifest asked for. */
    data class Approved(val token: String, val granted: List<String>) : Approval()

    /** Answered no. */
    data object Denied : Approval()

    /** Spent, expired, or never a pairing code at all. No amount of
     * polling changes it; the way on is a fresh code. */
    data class Over(val reason: String) : Approval()

    /** The core never answered. Worth asking again. */
    data class Unreachable(val error: String) : Approval()
}

/** True while the conversation is still worth another poll. */
fun pollingOn(state: Approval): Boolean =
    state is Approval.Waiting || state is Approval.Unreachable

/**
 * Redeem a code: say who this app is and what it wants. The core raises
 * the request on the operator's screen; nothing is granted here.
 */
suspend fun requestPairing(
    api: CoreApi,
    client: String,
    requested: List<String>,
): Approval {
    val body = JSONObject()
        .put("client", client)
        .put("requested", JSONArray(requested))
    val r = api.request("POST", "/v1/pair/redeem", body.toString())
    return when {
        r.optInt("status") == 200 -> Approval.Waiting(
            r.optJSONObject("body")?.optJSONObject("body")?.optString("fingerprint")?.takeIf { it.isNotEmpty() }
        )
        // This app redeemed already and was interrupted before collecting.
        // The session knows where it stands, so go and ask it.
        r.optInt("status") == 409 -> Approval.Waiting()
        transportDown(r) -> Approval.Unreachable(why(r))
        r.optInt("status") == 403 -> Approval.Over(
            "that ticket does not carry a pairing code — cut a fresh one with erisdb pair"
        )
        else -> Approval.Over(why(r))
    }
}

/**
 * Ask the session where it stands and, on approval, put the token in the
 * caller's hands before returning.
 *
 * `keep` runs first and runs once. The core hands the token over exactly
 * once, so a token collected and then lost to a crash costs the user
 * another trip to the core — durability here matters more than anywhere
 * else in this app.
 */
suspend fun collect(api: CoreApi, keep: (String) -> Unit): Approval {
    val r = api.request("GET", "/v1/pair/status")
    if (transportDown(r)) return Approval.Unreachable(why(r))
    if (r.optInt("status") != 200) return Approval.Over(why(r))

    val body = r.getJSONObject("body")
    val granted = body.optJSONArray("granted")?.let { arr ->
        (0 until arr.length()).map { arr.getString(it) }
    } ?: emptyList()

    return when (body.optString("status")) {
        "pending", "requested" -> Approval.Waiting(body.optString("fingerprint").takeIf { it.isNotEmpty() })
        "denied" -> Approval.Denied
        // Spent. The core mints the token once, at collection, and marks
        // the session collected in the same revision-checked write — so a
        // replayed code, or a second phone racing the same one, gets this
        // and nothing else.
        "collected" -> Approval.Over(
            "this pairing code's token was collected already — pair again"
        )
        "approved" -> {
            val token = body.optString("token").ifEmpty { null }
                ?: return Approval.Over(
                    "this pairing code's token was collected already — pair again"
                )
            keep(token)
            Approval.Approved(token, granted)
        }
        else -> Approval.Over("this core reports a pairing state this app does not know")
    }
}
