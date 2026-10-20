package dev.eris.auth.client

import android.os.Binder
import android.os.IBinder
import android.os.Parcel

/**
 * ErisAuth's Binder protocol, written out rather than generated from AIDL, so that the app and
 * ErisAuth share one definition and an app vendors it as one file. Every call is one-way.
 *
 * To ErisAuth's service, interface [SERVICE]:
 * - [ASK]: a request id the app chose, the request as JSON, then the app's callback binder. The
 *   JSON is exactly the line an app sends on ErisAuth's agent socket:
 *   `{"op":"approve","v":2,"service","name","grants","serves","app"}` or `{"op":"snapshot","v":2}`.
 * - [WITHDRAW]: a request id. Only the app that asked can withdraw its request.
 *
 * To the app's callback, interface [CALLBACK]:
 * - [ANSWER]: the answer as JSON, exactly the agent socket's reply line:
 *   `{"approved":{"note","snapshot"}}`, `{"denied":"why"}`, `{"snapshot":{…}}` or `{"error":"why"}`.
 *
 * Android tells ErisAuth which app is calling, by the Binder calling uid: its package and the
 * SHA-256 of its signing certificate are what the person sees and what the note records. An app
 * whose callback dies withdraws its requests.
 */
object Wire {
    const val SERVICE = "dev.eris.auth.IErisAuth"
    const val CALLBACK = "dev.eris.auth.IErisAuthCallback"
    const val ASK = IBinder.FIRST_CALL_TRANSACTION
    const val WITHDRAW = IBinder.FIRST_CALL_TRANSACTION + 1
    const val ANSWER = IBinder.FIRST_CALL_TRANSACTION

    private fun send(to: IBinder, descriptor: String, code: Int, write: (Parcel) -> Unit) {
        val data = Parcel.obtain()
        try {
            data.writeInterfaceToken(descriptor)
            write(data)
            to.transact(code, data, null, IBinder.FLAG_ONEWAY)
        } finally {
            data.recycle()
        }
    }

    fun ask(service: IBinder, requestId: String, requestJson: String, callback: IBinder) =
        send(service, SERVICE, ASK) {
            it.writeString(requestId)
            it.writeString(requestJson)
            it.writeStrongBinder(callback)
        }

    fun withdraw(service: IBinder, requestId: String) = send(service, SERVICE, WITHDRAW) { it.writeString(requestId) }

    fun answer(callback: IBinder, json: String) = send(callback, CALLBACK, ANSWER) { it.writeString(json) }

    /** An app's end of one request: hears the answer. */
    abstract class Callback : Binder() {
        abstract fun onAnswer(json: String)

        override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
            if (code != ANSWER) return super.onTransact(code, data, reply, flags)
            data.enforceInterface(CALLBACK)
            onAnswer(data.readString() ?: return true)
            return true
        }
    }

    /** ErisAuth's end: hears requests and withdrawals, each with the uid and pid Android says sent it. */
    abstract class Service : Binder() {
        abstract fun onAsk(requestId: String, requestJson: String, callback: IBinder, uid: Int, pid: Int)
        abstract fun onWithdraw(requestId: String, uid: Int)

        override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
            if (code != ASK && code != WITHDRAW) return super.onTransact(code, data, reply, flags)
            data.enforceInterface(SERVICE)
            val uid = getCallingUid()
            when (code) {
                ASK -> {
                    val requestId = data.readString() ?: return true
                    val requestJson = data.readString() ?: return true
                    val callback = data.readStrongBinder() ?: return true
                    onAsk(requestId, requestJson, callback, uid, getCallingPid())
                }
                WITHDRAW -> onWithdraw(data.readString() ?: return true, uid)
            }
            return true
        }
    }
}
