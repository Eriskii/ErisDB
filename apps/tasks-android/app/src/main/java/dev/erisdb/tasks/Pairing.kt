package dev.erisdb.tasks

import org.json.JSONObject

// Pairing is the front door. A core cuts one ticket —
//
//     bezel://pair/<base64url-nopad(JSON)>
//
// — and it says where the core is and carries a pairing code to start a
// conversation with it. No pairing service exists anywhere, so nothing in
// the middle ever holds the mapping.
//
// What the ticket does not carry is authority. The code grants exactly
// `meta:pairing:redeem` and is good for minutes; Redeem.kt turns it into a
// token, and a human decides what that token holds.
//
// A ticket is still the whole address, so reading one is all-or-nothing:
// every refusal below is a refusal to store half a config and leave the
// app dialing something that cannot answer.

/** The ticket format this app reads. A ticket of any other version is
 * refused rather than guessed at. */
const val TICKET_VERSION = 1

private const val TICKET_PREFIX = "bezel://pair/"

/** A ticket, once read. `eid` and `url` are the two transports a core can
 * offer; at least one of them is present. `code` is the pairing code, not
 * a capability — it buys one redemption and expires in minutes. */
data class Ticket(
    val name: String?,
    val eid: String?,
    val url: String?,
    val code: String,
)

/** Reading a ticket either yields one or says what was wrong with it. */
sealed class Pairing {
    data class Ok(val ticket: Ticket) : Pairing()

    /** `reason` is shown to the human, so it names the fault. */
    data class Refused(val reason: String) : Pairing()
}

/** Read a ticket as docs/pairing.md defines one. */
fun readTicket(text: String): Pairing {
    val trimmed = text.trim()
    if (!trimmed.startsWith(TICKET_PREFIX)) {
        return Pairing.Refused("that is not a pairing code — one starts with $TICKET_PREFIX")
    }
    val payload = decodeBase64Url(trimmed.substring(TICKET_PREFIX.length))
        ?: return Pairing.Refused("that pairing code is damaged — it is not valid base64url")
    val json = try {
        JSONObject(payload)
    } catch (_: Exception) {
        return Pairing.Refused("that pairing code decodes to something that is not a ticket")
    }

    val version = if (json.has("v")) json.optInt("v", -1) else -1
    if (version != TICKET_VERSION) {
        val named = if (json.has("v")) "version ${json.opt("v")}" else "no version"
        return Pairing.Refused("that pairing code carries $named; this app reads version $TICKET_VERSION")
    }

    val code = json.optString("token").trim()
    if (code.isEmpty()) return Pairing.Refused("that pairing code carries no token")

    val url = json.optString("url").trim().ifEmpty { null }
    val rawEid = json.optString("eid").trim()
    val eid = if (rawEid.isEmpty() && !json.has("eid")) null else rawEid.lowercase()
    if (eid != null && !(eid.length == 64 && eid.all { it in "0123456789abcdef" })) {
        return Pairing.Refused(
            "that pairing code's endpoint id is not 64 hex characters (it is ${eid.length})"
        )
    }
    if (eid == null && url == null) {
        return Pairing.Refused(
            "that pairing code names no core — it carries neither an endpoint id nor a web address"
        )
    }

    return Pairing.Ok(Ticket(json.optString("name").ifEmpty { null }, eid, url, code))
}

/**
 * Read a ticket this app can actually act on.
 *
 * A core that offers only `url` is reachable over plain HTTP on its LAN,
 * which is a browser's transport. This app dials over iroh and speaks
 * QUIC, so it needs the endpoint id — and says so rather than storing an
 * address it will never call.
 */
fun quicTicket(text: String): Pairing = when (val read = readTicket(text)) {
    is Pairing.Refused -> read
    is Pairing.Ok ->
        if (read.ticket.eid != null) read
        else Pairing.Refused(
            "that core offers only a web address (${read.ticket.url}) — " +
                "this app dials over iroh and needs an endpoint id"
        )
}

/** True when this install holds a core to dial and a capability to dial it
 * with. Anything less is unpaired, and pairing is the front door. */
fun paired(server: String, token: String): Boolean =
    server.isNotBlank() && token.isNotBlank()

/** What a `bezel://pair/…` arrival means to an app in a given state. */
sealed class Arrival {
    /** Nothing is paired, so the ticket applies where it lands. */
    data class Pair(val ticket: Ticket) : Arrival()

    /**
     * A core is already paired and working. Replacing it throws away a
     * cache and a cursor, so it is asked for first. `sameCore` is true
     * when the ticket names the core already paired and so brings only a
     * second pairing of the same one.
     */
    data class Confirm(val ticket: Ticket, val sameCore: Boolean) : Arrival()

    data class Refused(val reason: String) : Arrival()
}

/** Route an arriving ticket. Pure: the caller supplies what is stored. */
fun arrival(text: String, server: String, token: String): Arrival =
    when (val read = quicTicket(text)) {
        is Pairing.Refused -> Arrival.Refused(read.reason)
        is Pairing.Ok ->
            if (!paired(server, token)) Arrival.Pair(read.ticket)
            else Arrival.Confirm(read.ticket, read.ticket.eid == server)
    }

/** What an install does when it opens. */
sealed class Launch {
    /**
     * Dial the core already paired, then ask what this token holds.
     *
     * A token on disk is a token the core still honours: what changed is
     * how permissions are named, not the tokens themselves. An install
     * that was working keeps working, and `GET /v1/permissions` fills in
     * what it may do once it is connected.
     */
    data class Resume(val server: String, val token: String) : Launch()

    /** Never paired, or the sealed store lost the token — which reads the
     * same way, and wants the same front door. */
    data object Pair : Launch()
}

fun launch(server: String, token: String): Launch =
    if (paired(server, token)) Launch.Resume(server, token) else Launch.Pair

private const val B64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"

/**
 * RFC 4648 §5, unpadded, tolerating padding that should not be there.
 *
 * Written out rather than borrowed: `android.util.Base64` is a throwing
 * stub under JVM unit tests and `java.util.Base64` arrives at API 26,
 * above this app's floor. Sixteen lines buy the same decoder on the phone
 * and on the test runner.
 */
private fun decodeBase64Url(s: String): String? {
    val body = s.trimEnd('=')
    if (body.isEmpty() || body.length % 4 == 1) return null
    val out = ByteArray(body.length * 6 / 8)
    var written = 0
    var buffer = 0
    var bits = 0
    for (c in body) {
        val value = B64URL.indexOf(c)
        if (value < 0) return null
        buffer = (buffer shl 6) or value
        bits += 6
        if (bits >= 8) {
            bits -= 8
            out[written++] = ((buffer shr bits) and 0xff).toByte()
        }
    }
    return String(out, 0, written, Charsets.UTF_8)
}
