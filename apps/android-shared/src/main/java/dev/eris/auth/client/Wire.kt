package dev.eris.auth.client

import android.os.Binder
import android.os.IBinder
import android.os.Parcel

/**
 * The app's half of ErisAuth's Binder protocol, as ErisAuth's client library defines it. Every
 * call is one-way.
 *
 * To the service (`dev.eris.auth.IErisAuth`):
 * - [ASK]: the ask as JSON, then the app's callback binder.
 * - [WITHDRAW]: a request id.
 *
 * To the app's callback (`dev.eris.auth.IErisAuthCallback`):
 * - [TICKET]: the request id, then the ticket to redeem.
 * - [ANSWER]: `{"approved": [...]}`, `{"denied": "why"}` or `{"error": "why"}`.
 */
object Wire {
    const val SERVICE = "dev.eris.auth.IErisAuth"
    const val CALLBACK = "dev.eris.auth.IErisAuthCallback"
    const val ASK = IBinder.FIRST_CALL_TRANSACTION
    const val WITHDRAW = IBinder.FIRST_CALL_TRANSACTION + 1
    const val TICKET = IBinder.FIRST_CALL_TRANSACTION
    const val ANSWER = IBinder.FIRST_CALL_TRANSACTION + 1

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

    fun ask(service: IBinder, ask: String, callback: IBinder) = send(service, SERVICE, ASK) {
        it.writeString(ask)
        it.writeStrongBinder(callback)
    }

    fun withdraw(service: IBinder, id: String) = send(service, SERVICE, WITHDRAW) { it.writeString(id) }

    /** An app's end: hears the ticket, then the answer. */
    abstract class Callback : Binder() {
        abstract fun onTicket(id: String, ticket: String)
        abstract fun onAnswer(answer: String)

        override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
            if (code != TICKET && code != ANSWER) return super.onTransact(code, data, reply, flags)
            data.enforceInterface(CALLBACK)
            when (code) {
                TICKET -> onTicket(data.readString() ?: return true, data.readString() ?: return true)
                ANSWER -> onAnswer(data.readString() ?: return true)
            }
            return true
        }
    }
}
