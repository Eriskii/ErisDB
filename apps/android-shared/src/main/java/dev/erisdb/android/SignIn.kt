package dev.erisdb.android

import android.content.Context
import android.os.Handler
import android.os.Looper
import dev.eris.eon.client.ErisOverlayNetwork
import org.json.JSONObject

// An app gets into ErisDB through ErisOverlayNetwork on this phone, and only so. It
// asks ErisOverlayNetwork to approve its key to use a database, `database/1`, with the
// grants it wants. ErisDB is what serves `database/1` in the mesh. The person
// approves in ErisOverlayNetwork — all of the grants or fewer — and types ErisDB's
// passphrase there; ErisOverlayNetwork enrolls the app at the mesh's ErisDB and
// answers, once ErisDB has taken it, with the app's note and a snapshot of the
// mesh. The snapshot says where ErisDB is. The app keeps that beside its key
// and dials it, then and on every later launch, with nothing but the key.

/** The capability every ErisDB app asks ErisOverlayNetwork to use: a database, which ErisDB serves. */
const val CAPABILITY = "database/1"

/** What the status line says once ErisDB stops letting this app in: its installation was
 * revoked, its device left the mesh, or its note was revoked. */
const val SHUT_OUT = "ErisDB no longer lets this app in · sign in again"

/** What this app asks ErisOverlayNetwork: to let the key whose endpoint id is [app], calling itself
 * [name], use the database with [grants]. It serves nothing. */
data class Ask(val name: String, val grants: List<String>, val app: String) {
    val uses: Map<String, List<String>> get() = mapOf(CAPABILITY to grants)

    /** The request as ErisOverlayNetwork hears it: the agent socket's approve line. */
    val line: String get() = ErisOverlayNetwork.approveJson(name, uses, emptyList(), app)
}

/** Where signing in stands. */
sealed class SignIn {
    /** Nothing asked, or the last request taken back. */
    data object Ready : SignIn()

    /** ErisOverlayNetwork is not installed on this phone, and it is the only way in. */
    data object NoErisOverlayNetwork : SignIn()

    /** ErisOverlayNetwork is asking the person. */
    data object Asking : SignIn()

    /** Let in, and ErisDB is at [server], kept beside the key. */
    data class SignedIn(val server: String) : SignIn()

    /** The person said no. */
    data class Denied(val why: String) : SignIn()

    /** ErisOverlayNetwork could not let this app in, or its approval did not say where ErisDB is. */
    data class Failed(val why: String) : SignIn()
}

/** The ErisDB a `nativeLocate` envelope names, as address JSON, or why it names none. */
fun located(envelope: String): Read<String> = runCatching {
    val answer = JSONObject(envelope)
    if (answer.optBoolean("ok")) {
        val server = answer.getJSONObject("server")
        if (serverId(server.toString()) == null) Read.Failed("ErisDB's address names no endpoint")
        else Read.Ok(server.toString())
    } else {
        Read.Failed(answer.optString("error").ifEmpty { "the mesh's ErisDB could not be found" })
    }
}.getOrElse { Read.Failed("the native client's answer could not be read") }

/**
 * What ErisOverlayNetwork's answer comes to. An approval's snapshot says where ErisDB is — [locate] is the
 * native client's reading of it — and [keep] has kept that address before this returns, so a
 * kill between here and the first dial loses nothing.
 */
fun signedIn(answer: ErisOverlayNetwork.Answer, locate: (String) -> String, keep: (String) -> Unit): SignIn = when (answer) {
    is ErisOverlayNetwork.Answer.Approved -> when (val at = located(locate(answer.snapshot))) {
        is Read.Ok -> {
            keep(at.value)
            SignIn.SignedIn(at.value)
        }
        is Read.Failed -> SignIn.Failed("ErisOverlayNetwork approved this app, but ${at.error}")
    }
    is ErisOverlayNetwork.Answer.Denied -> SignIn.Denied(answer.why)
    is ErisOverlayNetwork.Answer.Failed -> SignIn.Failed(answer.why)
}

/** The endpoint id a kept address names, in lowercase hex: the address JSON's `id`, or a bare
 * id; null when it names none. */
fun serverId(server: String): String? {
    val text = server.trim()
    val id = if (text.startsWith("{")) runCatching { JSONObject(text).optString("id") }.getOrNull() else text
    return id?.lowercase()?.takeIf { it.length == 64 && it.all { c -> c in "0123456789abcdef" } }
}

/**
 * One app's conversation with ErisOverlayNetwork. [ask] starts one, taking back any still open; its answer
 * arrives on the main thread, null when ErisOverlayNetwork is not installed or will not connect.
 */
class ErisOverlayNetworkSignIn(context: Context) {
    private val context = context.applicationContext
    private val main = Handler(Looper.getMainLooper())
    private var eon: ErisOverlayNetwork? = null
    private var request: ErisOverlayNetwork.Request? = null

    // Which conversation is current; anything an older one says is dropped.
    private var current = 0

    /** Whether ErisOverlayNetwork is installed on this phone. */
    val installed: Boolean get() = ErisOverlayNetwork.isInstalled(context)

    fun ask(ask: Ask, answered: (ErisOverlayNetwork.Answer?) -> Unit) {
        stop()
        val mine = current
        ErisOverlayNetwork.connect(context) { connected ->
            if (mine != current) {
                connected?.close()
                return@connect
            }
            if (connected == null) return@connect answered(null)
            eon = connected
            request = connected.approve(ask.name, ask.uses, app = ask.app) { answer ->
                main.post {
                    if (mine != current) return@post
                    // Answered: nothing is left to take back.
                    request = null
                    eon?.close()
                    eon = null
                    answered(answer)
                }
            }
        }
    }

    /** Take back a request still open, and let ErisOverlayNetwork go. */
    fun stop() {
        current += 1
        request?.withdraw()
        request = null
        eon?.close()
        eon = null
    }
}
