package dev.erisdb.tasks

import dev.erisdb.android.*

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
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LifecycleResumeEffect
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.util.UUID

/** The facet, which is also the permission namespace: `tasks:read` and
 * friends. The schema version lives in the registration body, not here,
 * so a grant survives a schema upgrade. */
const val FACET = "tasks"

/** What this app calls itself to ErisOverlayNetwork, and in `source`. */
const val CLIENT = "Tasks (Android) v0.4"

/** What this app is called on its own screens. */
private const val APP = "Tasks"

// ---------------------------------------------------------------- root

private sealed class Screen {
    data object Main : Screen()
    data class Editor(val item: JSONObject?) : Screen()
    data object Settings : Screen()

    /** The front door, shown to a phone ErisDB has not let this app in from. */
    data object SignIn : Screen()
}

@Composable
fun TasksApp() {
    val ctx = LocalContext.current
    val store = remember { Store(ctx) }
    val gate = remember { SyncGate() }
    val scope = rememberCoroutineScope()
    // ErisOverlayNetwork on this phone: the way in.
    val eon = remember { ErisOverlayNetworkSignIn(ctx) }

    // Where this app's ErisDB is, once ErisOverlayNetwork has let it in.
    var server by remember { mutableStateOf(store.credentials.server) }
    var signIn by remember { mutableStateOf<SignIn>(if (eon.installed) SignIn.Ready else SignIn.NoErisOverlayNetwork) }
    // What ErisDB last said this app holds. Unknown until it answers, and
    // unknown draws every button — a 403 is what corrects a guess, not a
    // greyed-out Add on a phone that simply has no signal yet.
    var grants by remember { mutableStateOf(Grants(store.grants)) }
    var connected by remember { mutableStateOf(false) }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf("not connected") }
    var snapshot by remember { mutableStateOf(store.loadSnapshot()) }
    var outbox by remember { mutableStateOf(store.ops()) }
    var aliases by remember { mutableStateOf(store.aliases()) }
    // The front door is for a phone that has never been let in. One that
    // has opens straight onto its tasks.
    var screen by remember { mutableStateOf<Screen>(if (server != null) Screen.Main else Screen.SignIn) }
    var syncTick by remember { mutableStateOf(0) }
    var connectTick by remember { mutableStateOf(0) }
    // Advances every poll so "today 14:00" and the overdue colouring stay
    // true without the caller threading a clock through every row.
    var nowMs by remember { mutableStateOf(System.currentTimeMillis()) }

    val askToNotify = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { }

    /** Bring the screen in line with an ErisDB just kept beside the key, and dial it. */
    fun take(kept: String, moved: Boolean) {
        server = kept
        // A new approval holds whatever the person just chose; what the
        // last one held says nothing about it.
        store.grants = null
        grants = Grants.UNKNOWN
        // Another ErisDB means another item store and another feed, so the
        // cached snapshot and its cursor go.
        if (moved) snapshot = null
        // The native client still holds the previous dial; only connect()
        // dials again. Force it even when a stale connection looks healthy.
        connected = false
        connectTick += 1
        screen = Screen.Main
    }

    /**
     * Ask ErisOverlayNetwork on this phone to let this app in. The address its
     * approval names is kept beside the key before anything is dialed.
     */
    fun askErisOverlayNetwork() {
        scope.launch {
            val app = withContext(Dispatchers.IO) { ErisDB.endpointId(store.credentials.key()) }
                ?: return@launch run { signIn = SignIn.Failed("this app's key cannot be read") }
            signIn = SignIn.Asking
            eon.ask(Ask(CLIENT, MANIFEST, app)) { answer ->
                if (answer == null) return@ask run { signIn = SignIn.NoErisOverlayNetwork }
                scope.launch {
                    var moved = false
                    val next = withContext(Dispatchers.IO) {
                        signedIn(answer, ErisDB::locate) { moved = store.credentials.keep(it) }
                    }
                    signIn = next
                    if (next is SignIn.SignedIn) take(next.server, moved)
                }
            }
        }
    }

    /** Take back the request ErisOverlayNetwork is asking the person about. */
    fun cancelSignIn() {
        eon.stop()
        signIn = SignIn.Ready
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
     * One pass: check what this app holds, drain the outbox, then take
     * whatever the change feed has. Serialized — two of these at once
     * would send the same queued op twice.
     */
    suspend fun sync() = gate.serialized {
        val syncingServer = server
        if (!connected || syncingServer == null) return@serialized
        withContext(Dispatchers.IO) {
            when (val current = fetchGrants(ErisDBApi)) {
                is Read.Ok -> {
                    store.grants = current.value.held
                    withContext(Dispatchers.Main) { grants = current.value }
                }
                is Read.Failed -> if (current.status == 401) {
                    withContext(Dispatchers.Main) {
                        status = SHUT_OUT
                        grants = Grants(emptyList())
                    }
                    return@withContext
                }
            }

            // Initialize before draining, after the live grant check.
            // A setup failure must not erase the user's pending creations.
            if (store.ops().any { it.optString("op") == "create" }) {
                val setupError = ensureFacet(ErisDBApi, grants)
                if (setupError != null) {
                    withContext(Dispatchers.Main) {
                        status = "setup pending: $setupError · ${store.ops().size} queued"
                    }
                    return@withContext
                }
            }

            val drop = drainOutbox(store, ErisDBApi, FACET)

            val previous = snapshot
            val working = LinkedHashMap(previous?.items.orEmpty())
            var cursor = previous?.cursor
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
            if (server != syncingServer) return@withContext
            val updated = if (error == null) Snapshot(working, checkNotNull(cursor)) else null
            val diskError = updated?.let {
                runCatching { store.saveSnapshot(syncingServer, it) }.exceptionOrNull()
            }
            if (updated != null) announceDue(ctx, store, working, now)

            withContext(Dispatchers.Main) main@{
                if (server != syncingServer) return@main
                outbox = store.ops()
                aliases = store.aliases()
                nowMs = now
                if (updated != null) {
                    snapshot = updated
                    status = diskError?.let { "could not save offline copy: ${it.message}" }
                        ?: drop ?: if (outbox.isEmpty()) "synced" else "${outbox.size} pending"
                } else {
                    status = "offline: $error" +
                        if (outbox.isEmpty()) "" else " · ${outbox.size} queued"
                }
            }
        }
    }

    /** Dial the ErisDB kept beside the key, then ask it what this app holds. */
    suspend fun connect() {
        val at = server ?: return
        gate.serialized {
            withContext(Dispatchers.IO) {
                withContext(Dispatchers.Main) { connecting = true; status = "dialing…" }
                val err = ErisDB.dial(at, CLIENT, store.credentials.key())
                if (err == null) {
                    // The person may have approved less than this app asked
                    // for, so ask ErisDB what it holds before drawing a
                    // single button that writes.
                    val held = when (val asked = fetchGrants(ErisDBApi)) {
                        is Read.Ok -> asked.value.also { store.grants = it.held }
                        is Read.Failed -> Grants(store.grants)
                    }
                    withContext(Dispatchers.Main) { grants = held }
                    withContext(Dispatchers.Main) { connected = true; status = "syncing…" }
                } else {
                    withContext(Dispatchers.Main) { status = err }
                }
                withContext(Dispatchers.Main) { connecting = false }
            }
        }
        if (connected) sync()
    }

    // Dial what was kept; poll while connected. A phone never let in asks
    // ErisOverlayNetwork straight away.
    LaunchedEffect(Unit) {
        ensureDueChannel(ctx)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            askToNotify.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (server != null) connect() else if (eon.installed) askErisOverlayNetwork()
        while (true) {
            delay(10_000)
            nowMs = System.currentTimeMillis()
            if (connected) sync() else if (server != null) connect()
        }
    }
    LaunchedEffect(syncTick) { if (syncTick > 0 && connected) sync() }

    // Back from installing ErisOverlayNetwork: ask it.
    LifecycleResumeEffect(Unit) {
        if (server == null && signIn is SignIn.NoErisOverlayNetwork && eon.installed) askErisOverlayNetwork()
        onPauseOrDispose { }
    }

    val shown = remember(snapshot, outbox, aliases) {
        taskOrder(applyPending(snapshot?.items?.values.orEmpty(), outbox, aliases))
    }

    // The front door has nothing behind it: back from there leaves the app.
    BackHandler(enabled = screen != Screen.Main && screen != Screen.SignIn) { screen = Screen.Main }
    DisposableEffect(Unit) { onDispose { eon.stop() } }

    // Sub-screens slide in from the right; going back slides them out.
    fun depth(s: Screen) = if (s is Screen.Main || s is Screen.SignIn) 0 else 1
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
            haveData = snapshot != null || outbox.isNotEmpty(),
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
            app = APP,
            server = server,
            signIn = signIn,
            grants = grants,
            onSignIn = ::askErisOverlayNetwork,
            onCancel = ::cancelSignIn,
            onBack = { screen = Screen.Main },
        )
        is Screen.SignIn -> SignInScreen(
            app = APP,
            state = signIn,
            asked = MANIFEST.map(::describeGrant),
            onAsk = ::askErisOverlayNetwork,
            onCancel = ::cancelSignIn,
        )
        }
    }

    // Dial again whenever ErisOverlayNetwork lets this app in anew — a new approval is
    // a command, not a condition.
    LaunchedEffect(connectTick) {
        if (connectTick > 0 && server != null && !connecting) connect()
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
