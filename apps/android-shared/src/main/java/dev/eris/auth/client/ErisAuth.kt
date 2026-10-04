package dev.eris.auth.client

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.IBinder
import org.json.JSONArray
import org.json.JSONObject

/**
 * Signing an app in through ErisAuth on this phone, instead of showing the person a ticket.
 *
 * ```
 * ErisAuth.connect(context) { erisAuth ->
 *     if (erisAuth == null) { showScanOrPaste(); return@connect }
 *     erisAuth.ask("erisdb", "Notes", listOf("notes:read"), ErisAuth.Identity.iroh(myKeyHex), object : ErisAuth.Listener {
 *         override fun onTicket(ticket: String) = redeemWithMyKey(ticket)
 *         override fun onAnswer(answer: ErisAuth.Answer) = if (answer is ErisAuth.Answer.Approved) collect() else …
 *     })
 * }
 * ```
 *
 * The app redeems the ticket itself, with the key it named, exactly as it would a scanned one, and
 * collects its own credential from the service once approved: nothing it does later passes
 * through ErisAuth. Callbacks arrive on a binder thread.
 */
class ErisAuth private constructor(
    private val context: Context,
    private val connection: ServiceConnection,
    private val service: IBinder,
) {
    /** The key an app redeems its ticket with. */
    data class Identity(val kind: String, val key: String) {
        companion object {
            /** An Iroh key, as its endpoint id in hex. */
            fun iroh(endpointId: String) = Identity("iroh", endpointId)
        }
    }

    sealed class Answer {
        data class Approved(val granted: List<String>) : Answer()
        data class Denied(val why: String) : Answer()
        data class Failed(val why: String) : Answer()

        companion object {
            fun parse(json: String): Answer = runCatching {
                val answer = JSONObject(json)
                when {
                    answer.has("approved") -> answer.getJSONArray("approved").let { granted ->
                        Approved(List(granted.length()) { granted.getString(it) })
                    }
                    answer.has("denied") -> Denied(answer.getString("denied"))
                    else -> Failed(answer.optString("error", "ErisAuth sent no answer"))
                }
            }.getOrElse { Failed("ErisAuth sent something unreadable") }
        }
    }

    interface Listener {
        fun onTicket(ticket: String)
        fun onAnswer(answer: Answer)
    }

    /** One request; [withdraw] takes it back. */
    inner class Request internal constructor() {
        @Volatile internal var id: String? = null
        fun withdraw() {
            id?.let { runCatching { Wire.withdraw(service, it) } }
        }
    }

    /** Ask for [requested] at [service] as [client], to be redeemed with [identity]. */
    fun ask(service: String, client: String, requested: List<String>, identity: Identity, listener: Listener): Request {
        val request = Request()
        val callback = object : Wire.Callback() {
            override fun onTicket(id: String, ticket: String) {
                request.id = id
                listener.onTicket(ticket)
            }

            override fun onAnswer(answer: String) = listener.onAnswer(Answer.parse(answer))
        }
        Wire.ask(this.service, askJson(service, client, requested, identity), callback)
        return request
    }

    /** Let ErisAuth go. A request still open is withdrawn with the connection. */
    fun close() {
        runCatching { context.unbindService(connection) }
    }

    companion object {
        const val PACKAGE = "dev.eris.auth"
        const val ACTION = "dev.eris.auth.SIGN_IN"

        fun askJson(service: String, client: String, requested: List<String>, identity: Identity): String =
            JSONObject()
                .put("v", 1)
                .put("op", "pair")
                .put("service", service)
                .put("client", client)
                .put("requested", JSONArray(requested))
                .put("identity", JSONObject().put("kind", identity.kind).put("key", identity.key))
                .toString()

        private fun intent() = Intent(ACTION).setPackage(PACKAGE)

        /** Whether ErisAuth is installed here. */
        fun isInstalled(context: Context): Boolean =
            context.packageManager.resolveService(intent(), 0) != null

        /**
         * Connect to ErisAuth. [ready] hears it, or null when it is not installed or will not
         * connect, in which case the app pairs by ticket as it always can.
         */
        fun connect(context: Context, ready: (ErisAuth?) -> Unit) {
            val app = context.applicationContext
            val connection = object : ServiceConnection {
                private var delivered = false

                override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                    if (delivered) return
                    delivered = true
                    ready(ErisAuth(app, this, binder))
                }

                override fun onServiceDisconnected(name: ComponentName) {}

                override fun onNullBinding(name: ComponentName) {
                    if (delivered) return
                    delivered = true
                    runCatching { app.unbindService(this) }
                    ready(null)
                }
            }
            val bound = isInstalled(app) && runCatching {
                app.bindService(intent(), connection, Context.BIND_AUTO_CREATE)
            }.getOrDefault(false)
            if (!bound) ready(null)
        }
    }
}
