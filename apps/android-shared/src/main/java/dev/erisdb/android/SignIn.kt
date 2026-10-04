package dev.erisdb.android

import android.content.Context
import android.os.Handler
import android.os.Looper
import dev.eris.auth.client.ErisAuth

// ErisAuth on this phone is a shortcut past the front door. The app asks it
// for the permissions it needs; ErisAuth cuts a pairing at the core and
// hands back the ticket; the app redeems that ticket with its own key,
// exactly as it would a scanned one; and the person approves in ErisAuth's
// notification instead of on the core. The credential is collected from the
// core as after any pairing, and nothing the app does afterwards passes
// through ErisAuth.
//
// Without ErisAuth, or when it says no, the app pairs by ticket.

/** What ErisAuth's side of signing in asks of this app. */
sealed class SignIn {
    /** ErisAuth cut this ticket for this app: take it as any scanned ticket. */
    data class Ticket(val text: String) : SignIn()

    /** ErisAuth will not sign this app in, and says why. The app pairs by ticket. */
    data class Declined(val why: String) : SignIn()
}

/** ErisAuth's answer, as this app acts on it. An approval needs nothing from
 * here — the credential is collected from the core — so only a refusal does. */
fun signInAnswer(answer: ErisAuth.Answer): SignIn? = when (answer) {
    is ErisAuth.Answer.Approved -> null
    is ErisAuth.Answer.Denied -> SignIn.Declined("ErisAuth: ${answer.why}")
    is ErisAuth.Answer.Failed -> SignIn.Declined("ErisAuth: ${answer.why}")
}

/** The service every ErisDB app names when it asks ErisAuth. */
const val ERISAUTH_SERVICE = "erisdb"

/**
 * One app's conversation with ErisAuth. [ask] starts it; what ErisAuth says
 * arrives on the main thread.
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

    /** Ask ErisAuth to sign this app in as [client], with [requested], redeeming with the key whose endpoint id is [endpointId]. */
    fun ask(client: String, requested: List<String>, endpointId: String, onEvent: (SignIn) -> Unit) {
        stop()
        val mine = current
        ErisAuth.connect(context) { connected ->
            if (mine != current) {
                connected?.close()
                return@connect
            }
            if (connected == null) {
                onEvent(SignIn.Declined("ErisAuth is not available on this phone"))
                return@connect
            }
            erisAuth = connected
            request = connected.ask(
                ERISAUTH_SERVICE,
                client,
                requested,
                ErisAuth.Identity.iroh(endpointId),
                object : ErisAuth.Listener {
                    override fun onTicket(ticket: String) {
                        main.post { if (mine == current) onEvent(SignIn.Ticket(ticket)) }
                    }

                    override fun onAnswer(answer: ErisAuth.Answer) {
                        main.post {
                            if (mine != current) return@post
                            // Answered: nothing is left to withdraw.
                            request = null
                            signInAnswer(answer)?.let(onEvent)
                        }
                    }
                },
            )
        }
    }

    /** Withdraw a request still open, and let ErisAuth go. */
    fun stop() {
        request?.withdraw()
        finish()
    }

    /** Let ErisAuth go, once pairing has ended at the core. Withdrawing then
     * would ask ErisAuth to deny a pairing already settled. */
    fun finish() {
        current += 1
        request = null
        erisAuth?.close()
        erisAuth = null
    }
}
