package dev.erisdb.tasks

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import dev.erisdb.client.ErisDB
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.util.UUID

/** The facet, which is also the permission namespace: `tasks:read` and
 * friends. The schema version lives in the registration body, not here,
 * so a grant survives a schema upgrade. */
const val FACET = "tasks"

/** What this app calls itself on the pairing prompt and in `source`. */
const val CLIENT = "Tasks (Android) v0.4"

// ---------------------------------------------------------------- capability

private class Refreshed(val token: String?, val dead: Boolean)

/** Refresh when less than half the recorded lifetime remains, always
 * asking for the lifetime the admin chose at mint time. Transport
 * failures stay silent — the next sync re-evaluates from scratch. */
private fun maybeRefresh(store: Store): Refreshed {
    val none = Refreshed(null, false)
    val ttl = store.ttl
    if (ttl <= 0) return none
    val exp = tokenExp(store.token) ?: return none
    if (exp - System.currentTimeMillis() / 1000 >= ttl / 2) return none
    val r = ErisDB.refreshCapability(ttl)
    if (!r.optBoolean("ok")) return Refreshed(null, refreshRejected(r))
    val fresh = r.getString("token")
    store.token = fresh
    return Refreshed(fresh, false)
}

// ---------------------------------------------------------------- root

private sealed class Screen {
    data object Main : Screen()
    data class Editor(val item: JSONObject?) : Screen()
    data object Settings : Screen()

    /** The front door, shown to a phone that is not paired with anything. */
    data object Pairing : Screen()

    /** A code redeemed, a human deciding on another machine. */
    data object Waiting : Screen()
}

/**
 * @param pairUri the `bezel://pair/…` this app was opened with, from the
 *   camera app's scan or any other link — null on an ordinary launch.
 * @param onPairHandled called once that ticket has been acted on, so the
 *   same one does not land twice.
 */
@Composable
fun TasksApp(pairUri: String? = null, onPairHandled: () -> Unit = {}) {
    val ctx = LocalContext.current
    val store = remember { Store(ctx) }
    val gate = remember { SyncGate() }

    var server by remember { mutableStateOf(store.server) }
    var token by remember { mutableStateOf(store.token) }
    var coreName by remember { mutableStateOf(store.coreName) }
    // What the core last said this token holds. Unknown until it answers,
    // and unknown draws every button — a 403 is what corrects a guess,
    // not a greyed-out Add on a phone that simply has no signal yet.
    var grants by remember { mutableStateOf(Grants(store.grants)) }
    var connected by remember { mutableStateOf(false) }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf("not connected") }
    var cache by remember { mutableStateOf<Map<String, JSONObject>>(store.loadItems()) }
    var haveData by remember { mutableStateOf(store.hasCache()) }
    var outbox by remember { mutableStateOf(store.ops()) }
    var aliases by remember { mutableStateOf(store.aliases()) }
    // Pairing is the front door — but only for a phone that has never
    // walked through it. An install that already holds a core and a token
    // opens straight onto its tasks.
    var screen by remember {
        mutableStateOf<Screen>(
            when (launch(store.server, store.token)) {
                is Launch.Resume -> Screen.Main
                is Launch.Pair -> Screen.Pairing
            }
        )
    }
    // A ticket that would replace a working config, waiting on an answer.
    var confirming by remember { mutableStateOf<Arrival.Confirm?>(null) }
    // The pairing conversation in flight: the ticket being redeemed, and
    // where the human on the other machine has got to.
    var redeeming by remember { mutableStateOf<Ticket?>(null) }
    var approval by remember { mutableStateOf<Approval>(Approval.Waiting()) }
    // Why the last ticket was refused. Loud, because a ticket that cannot
    // be read is a ticket that changed nothing.
    var refused by remember { mutableStateOf<String?>(null) }
    var syncTick by remember { mutableStateOf(0) }
    var connectTick by remember { mutableStateOf(0) }
    // Advances every poll so "today 14:00" and the overdue colouring stay
    // true without the caller threading a clock through every row.
    var nowMs by remember { mutableStateOf(System.currentTimeMillis()) }

    val askToNotify = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { }

    /**
     * Write a core and a capability to disk, and say whether that moved
     * this phone to a different core.
     *
     * Durable before it returns. As soon as collection returns a token,
     * it belongs on disk with the core it
     * opens — a kill between here and the first sync then costs a cache,
     * not another trip to the core.
     */
    fun keep(newServer: String, newToken: String, name: String?): Boolean {
        val life = lifetimeOf(tokenExp(newToken), System.currentTimeMillis() / 1000)
        // A different core means a different item store and a different
        // feed, so the cursor goes with it — and so does the label, which
        // describes the core that is leaving.
        val moved = newServer != store.server
        if (moved) store.cursor = null
        store.server = newServer
        store.token = newToken
        store.coreName = name ?: if (moved) "" else store.coreName
        store.ttl = if (life is Lifetime.Seconds) life.value else 0L
        // A fresh token holds whatever a human just decided; what the last
        // one held says nothing about it.
        store.grants = null
        return moved
    }

    /** Bring the screen in line with credentials already on disk. */
    fun take(moved: Boolean) {
        server = store.server
        token = store.token
        coreName = store.coreName
        grants = Grants.UNKNOWN
        if (moved) {
            cache = emptyMap()
            haveData = false
        }
        // The native client still holds the previous credentials; only
        // connect() feeds it these. Force the reconfigure even when a
        // stale connection looks healthy.
        connected = false
        connectTick += 1
        screen = Screen.Main
    }

    /**
     * Take up a capability typed in by hand. A pasted token is already a
     * capability, so there is nobody to ask.
     */
    fun adopt(newServer: String, newToken: String, name: String?) {
        if (lifetimeOf(tokenExp(newToken), System.currentTimeMillis() / 1000) is Lifetime.Dead) {
            // `--ttl 0` mints exp == now. Saying so beats recording a
            // one-second lifetime the app can never refresh out of.
            status = "that token has already expired · mint one with --ttl 86400"
            return
        }
        take(keep(newServer, newToken, name))
    }

    /** Redeem a ticket's code and go and wait for a person. */
    fun beginPairing(ticket: Ticket) {
        connected = false
        approval = Approval.Pending
        redeeming = ticket
        screen = Screen.Waiting
    }

    /**
     * Act on a `bezel://pair/…` ticket, from a scan or a paste. A ticket
     * that would replace a working config is asked about first; one that
     * cannot be read says what was wrong with it and changes nothing.
     */
    fun takeTicket(text: String) {
        when (val landed = arrival(text, store.server, store.token)) {
            is Arrival.Refused -> refused = landed.reason
            is Arrival.Pair -> beginPairing(landed.ticket)
            is Arrival.Confirm -> confirming = landed
        }
    }

    /** Durable-first mutation: commit to the outbox, show it, then sync. */
    fun mutate(op: JSONObject) {
        store.enqueue(op)
        outbox = store.ops()
        syncTick += 1
    }

    fun update(item: JSONObject, body: JSONObject) {
        mutate(
            JSONObject()
                .put("op", "update")
                .put("id", item.getString("id"))
                .put("revision", item.optLong("revision"))
                .put("body", body)
        )
    }

    /**
     * One pass: refresh the token if it is halfway through its life, drain
     * the outbox, then take whatever the change feed has. Serialized — two
     * of these at once would send the same queued op twice.
     */
    suspend fun sync() = gate.serialized {
        if (!connected || redeeming != null) return@serialized
        withContext(Dispatchers.IO) {
            val refreshed = maybeRefresh(store)
            if (refreshed.token != null || refreshed.dead) {
                withContext(Dispatchers.Main) {
                    refreshed.token?.let { token = it }
                    if (refreshed.dead) { status = "access revoked or expired · pair again"; grants = Grants(emptyList()) }
                }
            }

            if (refreshed.dead) return@withContext
            when (val current = fetchGrants(ErisDBApi)) {
                is Read.Ok -> {
                    store.grants = current.value.held
                    withContext(Dispatchers.Main) { grants = current.value }
                }
                is Read.Failed -> if (current.status == 401) {
                    withContext(Dispatchers.Main) {
                        status = "access revoked or expired · pair again"
                        grants = Grants(emptyList())
                    }
                    return@withContext
                }
            }

            val drop = drainOutbox(store, ErisDBApi, FACET)

            val working = LinkedHashMap(cache)
            var cursor = store.cursor
            var error: String? = null
            if (cursor == null) {
                when (val seeded = seed(ErisDBApi, FACET)) {
                    is Read.Ok -> {
                        working.clear()
                        working.putAll(seeded.value.items)
                        cursor = seeded.value.cursor
                    }
                    is Read.Failed -> error = seeded.error
                }
            } else {
                when (val polled = pollChanges(ErisDBApi, FACET, cursor, working)) {
                    is Read.Ok -> cursor = polled.value
                    is Read.Failed -> error = polled.error
                }
            }

            val now = System.currentTimeMillis()
            if (error == null) {
                store.saveItems(working)
                store.cursor = cursor
                announceDue(ctx, store, working, now)
            }

            withContext(Dispatchers.Main) {
                outbox = store.ops()
                aliases = store.aliases()
                nowMs = now
                if (error == null) {
                    cache = working
                    haveData = true
                    status = drop ?: if (outbox.isEmpty()) "synced" else "${outbox.size} pending"
                } else {
                    status = "offline: $error" +
                        if (outbox.isEmpty()) "" else " · ${outbox.size} queued"
                }
            }
        }
    }

    suspend fun connect() {
        gate.serialized {
            if (redeeming != null) return@serialized
            withContext(Dispatchers.IO) {
                withContext(Dispatchers.Main) { connecting = true; status = "dialing…" }
                val err = ErisDB.configure(server, token, CLIENT, store.identityHex())
                if (err == null) {
                    // The human may have approved less than this app asked for,
                    // so ask the core what this token holds before drawing a
                    // single button that writes.
                    val held = when (val asked = fetchGrants(ErisDBApi)) {
                        is Read.Ok -> asked.value.also { store.grants = it.held }
                        is Read.Failed -> Grants(store.grants)
                    }
                    withContext(Dispatchers.Main) { grants = held }
                    ensureFacet(ErisDBApi, held)
                    withContext(Dispatchers.Main) { connected = true; status = "syncing…" }
                } else {
                    withContext(Dispatchers.Main) { status = err }
                }
                withContext(Dispatchers.Main) { connecting = false }
            }
        }
        if (connected && redeeming == null) sync()
    }

    /**
     * The pairing conversation: redeem the code, then poll until a person
     * answers. The token is written to the sealed store inside `collect`,
     * before this coroutine can be cancelled out from under it.
     */
    suspend fun runPairing(ticket: Ticket) = gate.serialized {
        connected = false
        val err = withContext(Dispatchers.IO) {
            ErisDB.configure(ticket.eid!!, ticket.code, CLIENT, store.identityHex())
        }
        if (err != null) {
            approval = Approval.Unreachable(err)
            return@serialized
        }
        var moved = false
        approval = withContext(Dispatchers.IO) { requestPairing(ErisDBApi, CLIENT, MANIFEST) }
        while (pollingOn(approval)) {
            delay(PAIR_POLL_MS)
            approval = withContext(Dispatchers.IO) {
                if (approval is Approval.Pending) requestPairing(ErisDBApi, CLIENT, MANIFEST)
                else collect(ErisDBApi) { fresh -> moved = keep(ticket.eid!!, fresh, ticket.name) }
            }
        }
        if (approval is Approval.Approved) {
            redeeming = null
            take(moved)
        }
    }

    // Auto-connect with saved config; poll while connected.
    LaunchedEffect(Unit) {
        ensureDueChannel(ctx)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            askToNotify.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (launch(server, token) is Launch.Resume) connect()
        while (true) {
            delay(10_000)
            nowMs = System.currentTimeMillis()
            if (connected) sync()
        }
    }
    LaunchedEffect(syncTick) { if (syncTick > 0 && connected) sync() }

    // One pairing conversation per ticket; cancelling clears the ticket,
    // which is what ends it.
    LaunchedEffect(redeeming) { redeeming?.let { runPairing(it) } }

    // A ticket scanned by the camera app arrives as the intent that opened
    // this one — on a cold start and, via onNewIntent, on a warm one.
    LaunchedEffect(pairUri) {
        pairUri?.let {
            takeTicket(it)
            onPairHandled()
        }
    }

    val shown = remember(cache, outbox, aliases) {
        taskOrder(applyPending(cache.values, outbox, aliases))
    }

    // The front door has nothing behind it: back from there leaves the app.
    BackHandler(
        enabled = screen != Screen.Main && screen != Screen.Pairing &&
            screen != Screen.Waiting
    ) { screen = Screen.Main }

    // Waiting is part of the front door, so back goes to it rather than
    // to a tasks screen this phone may not have earned yet.
    BackHandler(enabled = screen == Screen.Waiting) {
        redeeming = null
        screen = Screen.Pairing
    }

    // Sub-screens slide in from the right; going back slides them out.
    fun depth(s: Screen) =
        if (s is Screen.Main || s is Screen.Pairing || s is Screen.Waiting) 0 else 1
    AnimatedContent(
        targetState = screen,
        transitionSpec = {
            if (depth(targetState) > depth(initialState)) {
                (slideInHorizontally { it } + fadeIn())
                    .togetherWith(slideOutHorizontally { -it / 3 } + fadeOut())
            } else {
                (slideInHorizontally { -it / 3 } + fadeIn())
                    .togetherWith(slideOutHorizontally { it } + fadeOut())
            }
        },
        label = "screen",
    ) { s ->
        when (s) {
        is Screen.Main -> TasksScreen(
            items = shown,
            nowMs = nowMs,
            haveData = haveData,
            grants = grants,
            statusLine = if (status == "synced") null else status,
            onToggle = { item ->
                update(item, toggledBody(item.getJSONObject("body"), System.currentTimeMillis()))
            },
            onOpen = { screen = Screen.Editor(it) },
            onAdd = { screen = Screen.Editor(null) },
            onSettings = { screen = Screen.Settings },
        )
        is Screen.Editor -> EditorScreen(
            item = s.item,
            grants = grants,
            onSave = { body ->
                if (s.item == null) {
                    mutate(
                        JSONObject()
                            .put("op", "create")
                            .put("tmp", "$PENDING${UUID.randomUUID()}")
                            .put("body", body)
                    )
                } else {
                    update(s.item, body)
                }
                screen = Screen.Main
            },
            onDelete = {
                s.item?.let { mutate(JSONObject().put("op", "delete").put("id", it.getString("id"))) }
                screen = Screen.Main
            },
            onBack = { screen = Screen.Main },
        )
        is Screen.Settings -> SettingsScreen(
            coreName = coreName,
            server = server,
            token = token,
            status = status,
            connecting = connecting,
            grants = grants,
            onTicket = { takeTicket(it) },
            onConnect = { newServer, newToken -> adopt(newServer, newToken, null) },
            onBack = { screen = Screen.Main },
        )
        is Screen.Pairing -> PairingScreen(
            coreName = coreName,
            server = server,
            token = token,
            status = status,
            connecting = connecting,
            onTicket = { takeTicket(it) },
            onManual = { newServer, newToken -> adopt(newServer, newToken, null) },
            onBack = null,
        )
        is Screen.Waiting -> WaitingScreen(
            coreName = redeeming?.name ?: coreName,
            state = approval,
            requested = MANIFEST,
            onCancel = { redeeming = null; screen = Screen.Pairing },
            onBack = { redeeming = null; screen = Screen.Pairing },
        )
        }
    }

    refused?.let { reason ->
        AlertDialog(
            onDismissRequest = { refused = null },
            title = { Text("That pairing code was refused") },
            text = { Text(reason) },
            confirmButton = { TextButton(onClick = { refused = null }) { Text("OK") } },
        )
    }

    // A ticket over a working config is a decision, not a side effect.
    confirming?.let { pending ->
        val here = coreName.ifBlank { server.take(12) + "…" }
        val there = pending.ticket.name ?: (pending.ticket.eid?.take(12) + "…")
        AlertDialog(
            onDismissRequest = { confirming = null },
            title = { Text(if (pending.sameCore) "Pair with $here again?" else "Pair with $there?") },
            text = {
                Text(
                    if (pending.sameCore) {
                        "This code is for $here, the erisdb already paired. Redeeming it " +
                            "asks $here for permission again and replaces this phone's " +
                            "capability with whatever is approved; the tasks stay where " +
                            "they are."
                    } else {
                        "This phone is paired with $here. Pairing with $there drops the tasks " +
                            "cached from $here and syncs the new core from scratch."
                    }
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    beginPairing(pending.ticket)
                    confirming = null
                }) { Text(if (pending.sameCore) "Pair again" else "Replace") }
            },
            dismissButton = {
                TextButton(onClick = { confirming = null }) { Text("Keep $here") }
            },
        )
    }

    // Reconfigure whenever settings hand back credentials — Connect is a
    // command, not a condition.
    LaunchedEffect(connectTick) {
        if (connectTick > 0 && paired(server, token) && !connecting) connect()
    }
}

/** Announce every task that has come due since the last pass. The ledger
 * only advances when the notice could actually be posted, so a task that
 * came due before permission was granted still gets announced after. */
private fun announceDue(
    ctx: android.content.Context,
    store: Store,
    items: Map<String, JSONObject>,
    nowMs: Long,
) {
    if (!mayNotify(ctx)) return
    val due = dueNow(items.values, store.announced(), nowMs)
    for (item in due) notifyDue(ctx, item, nowMs)
    store.writeAnnounced(withAnnounced(store.announced(), due, items.keys))
}
