package dev.erisdb.client

import org.json.JSONObject

/**
 * The native erisdb client: real Iroh QUIC, HTTP/1.1 per bi-stream, the app's key its
 * credential. All calls block — invoke from Dispatchers.IO only.
 */
object ErisDB {
    init {
        System.loadLibrary("erisdb_client")
    }

    private external fun nativeEndpointId(keyHex: String): String?

    private external fun nativeLocate(snapshotJson: String): String?

    private external fun nativeDial(server: String, clientName: String, keyHex: String): String?

    private external fun nativeRequest(method: String, path: String, body: String?): String?

    /** The endpoint id the key dials as, in hex — the `app` this app asks ErisAuth to
     * approve — or null when the key is not 64 hex characters. */
    fun endpointId(keyHex: String): String? = nativeEndpointId(keyHex)?.ifEmpty { null }

    /** Where the mesh's ErisDB is, from a snapshot of the mesh:
     * `{"ok":true,"server":<address JSON>}` or `{"ok":false,"error":…}`. */
    fun locate(snapshot: String): String = nativeLocate(snapshot) ?: ""

    /** Dial ErisDB at [server] as the app holding [keyHex], replacing any client dialed
     * before. Null on success, why not otherwise. */
    fun dial(server: String, clientName: String, keyHex: String): String? {
        val answer = nativeDial(server, clientName, keyHex) ?: return UNANSWERED
        return answer.ifEmpty { null }
    }

    /** Response envelope: {"status": n, "body": …} or {"status": 0, "error": …}. */
    fun request(method: String, path: String, body: String? = null): JSONObject =
        nativeRequest(method, path, body)?.let(::JSONObject) ?: JSONObject().put("status", 0).put("error", UNANSWERED)

    /** What a call says when the JVM could not take its answer: out of memory. */
    private const val UNANSWERED = "the native client could not answer"
}
