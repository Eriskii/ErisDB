package dev.eris.eon.client

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.IBinder
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import org.json.JSONArray
import org.json.JSONObject

/**
 * Asking ErisOverlayNetwork on this phone to approve an app, as an app beside ErisOverlayNetwork on a computer asks
 * over its agent socket. Vendor this file and [Wire] as they are.
 *
 * ```
 * ErisOverlayNetwork.connect(context) { eon ->
 *     if (eon == null) return@connect askByTicket()          // no ErisOverlayNetwork on this phone
 *     eon.approve("Tasks", mapOf("database/1" to listOf("tasks:read")), app = myEndpointIdHex) { answer ->
 *         when (answer) {
 *             is ErisOverlayNetwork.Answer.Approved -> keep(answer.note, answer.snapshot)
 *             is ErisOverlayNetwork.Answer.Denied -> tellThePerson(answer.why)
 *             is ErisOverlayNetwork.Answer.Failed -> tellThePerson(answer.why)
 *         }
 *     }
 * }
 * ```
 *
 * An app asks for capabilities, named for what they are, `database/1`, `files/1`, not for the
 * programs serving them. The person sees the app as Android names it, by its package and the
 * SHA-256 of its signing certificate, and approves it as asked, approves part of its grants, or
 * denies it. An app using something whose server takes a passphrase, such as a database, is
 * approved only once the person has typed that passphrase and the server has accepted the app's
 * note. The note names [approve]'s `app`, the app's own Iroh key as its endpoint id in hex, and
 * only that key can use it. Answers arrive on a binder thread.
 */
class ErisOverlayNetwork private constructor(
    private val context: Context,
    private val connection: ServiceConnection,
    private val service: IBinder,
) {
    /**
     * Serving [capability]: [single] when a mesh has one server of it, the most recently approved;
     * [passphrase] when apps using it also need the server's passphrase.
     */
    data class Serves(val capability: String, val single: Boolean = false, val passphrase: Boolean = false)

    sealed class Answer {
        /** The app's note and a snapshot of the mesh, each as its JSON text. */
        data class Approved(val note: String, val snapshot: String) : Answer()
        data class Denied(val why: String) : Answer()
        data class Failed(val why: String) : Answer()

        companion object {
            /** An agent reply line as an answer. */
            fun parse(json: String): Answer = runCatching {
                val reply = JSONObject(json)
                when {
                    reply.has("approved") -> reply.getJSONObject("approved").let {
                        Approved(it.getJSONObject("note").toString(), it.getJSONObject("snapshot").toString())
                    }
                    reply.has("denied") -> Denied(reply.getString("denied"))
                    reply.has("error") -> Failed(reply.getString("error"))
                    else -> Failed("ErisOverlayNetwork answered something else: $json")
                }
            }.getOrElse { Failed("ErisOverlayNetwork sent something unreadable") }
        }
    }

    /** One request to ErisOverlayNetwork; [withdraw] takes it back. */
    inner class Request internal constructor(val id: String) {
        fun withdraw() {
            if (open.remove(id) != null) runCatching { Wire.withdraw(service, id) }
        }
    }

    /** The requests waiting for an answer, each with what to do with it. */
    private val open = ConcurrentHashMap<String, (String) -> Unit>()

    /** Should ErisOverlayNetwork go away, every open request fails. */
    private val gone = IBinder.DeathRecipient {
        val failed = """{"error":"ErisOverlayNetwork went away"}"""
        open.keys.toList().forEach { id -> open.remove(id)?.invoke(failed) }
    }

    init {
        runCatching { service.linkToDeath(gone, 0) }
    }

    private fun ask(line: String, answered: (String) -> Unit): Request {
        val id = UUID.randomUUID().toString()
        open[id] = answered
        val callback = object : Wire.Callback() {
            override fun onAnswer(json: String) {
                open.remove(id)?.invoke(json)
            }
        }
        runCatching { Wire.ask(service, id, line, callback) }.onFailure {
            open.remove(id)?.invoke(JSONObject().put("error", "could not reach ErisOverlayNetwork: ${it.message}").toString())
        }
        return Request(id)
    }

    /**
     * Ask to approve the app holding the key [app] (its endpoint id, in hex), under [name], to use
     * each capability in [uses] with the grants it lists, and to serve those in [serves].
     * [listener] hears the answer once.
     */
    fun approve(
        name: String,
        uses: Map<String, List<String>>,
        serves: List<Serves> = emptyList(),
        app: String,
        listener: (Answer) -> Unit,
    ): Request = ask(approveJson(name, uses, serves, app)) { listener(Answer.parse(it)) }

    /** The mesh as this phone's ErisOverlayNetwork has it, as a snapshot's JSON text; null when it has none. */
    fun snapshot(ready: (String?) -> Unit) {
        ask(SNAPSHOT) { reply -> ready(runCatching { JSONObject(reply).getJSONObject("snapshot").toString() }.getOrNull()) }
    }

    /** Let ErisOverlayNetwork go. Requests still open are withdrawn. */
    fun close() {
        open.keys.toList().forEach { Request(it).withdraw() }
        runCatching { service.unlinkToDeath(gone, 0) }
        runCatching { context.unbindService(connection) }
    }

    companion object {
        const val PACKAGE = "dev.eris.eon"
        const val ACTION = "dev.eris.eon.SIGN_IN"

        /** The version of the agent protocol these requests speak. */
        const val VERSION = 3

        /** The agent socket's snapshot request line. */
        val SNAPSHOT: String get() = JSONObject().put("op", "snapshot").put("v", VERSION).toString()

        /** The agent socket's approve request line. */
        fun approveJson(name: String, uses: Map<String, List<String>>, serves: List<Serves>, app: String): String {
            val used = JSONObject()
            uses.forEach { (capability, grants) -> used.put(capability, JSONArray(grants)) }
            val served = JSONArray()
            serves.forEach { s ->
                val one = JSONObject().put("capability", s.capability)
                if (s.single) one.put("single", true)
                if (s.passphrase) one.put("passphrase", true)
                served.put(one)
            }
            return JSONObject()
                .put("op", "approve")
                .put("v", VERSION)
                .put("name", name)
                .put("uses", used)
                .put("serves", served)
                .put("app", app)
                .toString()
        }

        private fun intent() = Intent(ACTION).setPackage(PACKAGE)

        /** Whether ErisOverlayNetwork is installed here. */
        fun isInstalled(context: Context): Boolean = context.packageManager.resolveService(intent(), 0) != null

        /**
         * Connect to ErisOverlayNetwork. [ready] hears it, or null when it is not installed or will not
         * connect, in which case the app asks a device by ticket instead.
         */
        fun connect(context: Context, ready: (ErisOverlayNetwork?) -> Unit) {
            val app = context.applicationContext
            val connection = object : ServiceConnection {
                private var delivered = false

                override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                    if (delivered) return
                    delivered = true
                    ready(ErisOverlayNetwork(app, this, binder))
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
