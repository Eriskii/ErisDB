package dev.erisdb.client

import org.json.JSONObject

/**
 * The native erisdb client: real Iroh QUIC, HTTP/1.1 per bi-stream.
 * All calls block — invoke from Dispatchers.IO only.
 */
object ErisDB {
    init {
        System.loadLibrary("erisdb_client")
    }

    private external fun nativeConfigure(
        server: String,
        token: String,
        clientName: String,
        identityHex: String,
    ): String

    private external fun nativeRequest(method: String, path: String, body: String?): String

    private external fun nativeRefreshCapability(ttlSecs: Long): String

    /** Returns null on success, an error message otherwise. */
    fun configure(server: String, token: String, clientName: String, identityHex: String): String? =
        nativeConfigure(server, token, clientName, identityHex).ifEmpty { null }

    /** Response envelope: {"status": n, "body": …} or {"status": 0, "error": …}. */
    fun request(method: String, path: String, body: String? = null): JSONObject =
        JSONObject(nativeRequest(method, path, body))

    /** Trade the current token for one with the same scope and a fresh
     * expiry. Envelope: {"ok":true,"token":…} or {"ok":false,"error":…}.
     * The native client swaps to the fresh token itself; persisting it is
     * the caller's job. */
    fun refreshCapability(ttlSecs: Long): JSONObject =
        JSONObject(nativeRefreshCapability(ttlSecs))
}
