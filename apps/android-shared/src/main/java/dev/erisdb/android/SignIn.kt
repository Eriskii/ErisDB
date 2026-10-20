package dev.erisdb.android

import android.content.Context
import android.os.Handler
import android.os.Looper
import dev.eris.auth.client.ErisAuth
import org.json.JSONObject

// An app gets into ErisDB through ErisAuth on this phone, and only so. It
// asks ErisAuth to approve its key for the service `erisdb`, as a client,
// with the grants it wants. The person approves in ErisAuth — all of them or
// fewer — and types ErisDB's passphrase there; ErisAuth enrolls the app at
// the mesh's ErisDB and answers, once ErisDB has taken it, with the app's
// note and a snapshot of the mesh. The snapshot says where ErisDB is. The
// app keeps that beside its key and dials it, then and on every later
// launch, with nothing but the key.

/** The service every ErisDB app asks ErisAuth for. */
const val SERVICE = "erisdb"

/** What the status line says once ErisDB stops letting this app in: its installation was
 * revoked, its device left the mesh, or its note was revoked. */
const val SHUT_OUT = "ErisDB no longer lets this app in · sign in again"

/** What this app asks ErisAuth: to let the key whose endpoint id is [app] into ErisDB, as a
 * client calling itself [name], with [grants]. */
data class Ask(val name: String, val grants: List<String>, val app: String) {
    val service: String get() = SERVICE

    /** A client: ErisDB serves, this app does not. */
    val serves: Boolean get() = false

    /** The request as ErisAuth hears it: the agent socket's approve line. */
    val line: String get() = ErisAuth.approveJson(service, name, grants, serves, app)
}

/** Where signing in stands. */
sealed class SignIn {
    /** Nothing asked, or the last request taken back. */
    data object Ready : SignIn()

    /** ErisAuth is not installed on this phone, and it is the only way in. */
    data object NoErisAuth : SignIn()

    /** ErisAuth is asking the person. */
    data object Asking : SignIn()

    /** Let in, and ErisDB is at [server], kept beside the key. */
    data class SignedIn(val server: String) : SignIn()

    /** The person said no. */
    data class Denied(val why: String) : SignIn()

    /** ErisAuth could not let this app in, or its approval did not say where ErisDB is. */
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
 * What ErisAuth's answer comes to. An approval's snapshot says where ErisDB is — [locate] is the
 * native client's reading of it — and [keep] has kept that address before this returns, so a
 * kill between here and the first dial loses nothing.
 */
fun signedIn(answer: ErisAuth.Answer, locate: (String) -> String, keep: (String) -> Unit): SignIn = when (answer) {
    is ErisAuth.Answer.Approved -> when (val at = located(locate(answer.snapshot))) {
        is Read.Ok -> {
            keep(at.value)
            SignIn.SignedIn(at.value)
        }
        is Read.Failed -> SignIn.Failed("ErisAuth approved this app, but ${at.error}")
    }
    is ErisAuth.Answer.Denied -> SignIn.Denied(answer.why)
    is ErisAuth.Answer.Failed -> SignIn.Failed(answer.why)
}

/** The endpoint id a kept address names, in lowercase hex: the address JSON's `id`, or a bare
 * id; null when it names none. */
fun serverId(server: String): String? {
    val text = server.trim()
    val id = if (text.startsWith("{")) runCatching { JSONObject(text).optString("id") }.getOrNull() else text
    return id?.lowercase()?.takeIf { it.length == 64 && it.all { c -> c in "0123456789abcdef" } }
}

/**
 * One app's conversation with ErisAuth. [ask] starts one, taking back any still open; its answer
 * arrives on the main thread, null when ErisAuth is not installed or will not connect.
 */
class ErisAuthSignIn(context: Context) {
    private val context = context.applicationContext
    private val main = Handler(Looper.getMainLooper())
    private var erisAuth: ErisAuth? = null
    private var request: ErisAuth.Request? = null

    // Which conversation is current; anything an older one says is dropped.
    private var current = 0

    /** Whether ErisAuth is installed on this phone. */
    val installed: Boolean get() = ErisAuth.isInstalled(context)

    fun ask(ask: Ask, answered: (ErisAuth.Answer?) -> Unit) {
        stop()
        val mine = current
        ErisAuth.connect(context) { connected ->
            if (mine != current) {
                connected?.close()
                return@connect
            }
            if (connected == null) return@connect answered(null)
            erisAuth = connected
            request = connected.approve(ask.service, ask.name, ask.grants, ask.serves, ask.app) { answer ->
                main.post {
                    if (mine != current) return@post
                    // Answered: nothing is left to take back.
                    request = null
                    erisAuth?.close()
                    erisAuth = null
                    answered(answer)
                }
            }
        }
    }

    /** Take back a request still open, and let ErisAuth go. */
    fun stop() {
        current += 1
        request?.withdraw()
        request = null
        erisAuth?.close()
        erisAuth = null
    }
}
