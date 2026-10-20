package dev.erisdb.lists

import dev.erisdb.android.*

import dev.erisdb.client.ErisDB
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.compose.LifecycleResumeEffect
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.util.UUID

/** The facet, which is also the permission namespace: `lists:read` and
 * friends. The schema version lives in the registration body, not here,
 * so a grant survives a schema upgrade. */
const val FACET = "lists"

/** What this app calls itself to ErisAuth, and in `source`. */
const val CLIENT = "Lists (Android) v0.5"

/** What this app is called on its own screens. */
private const val APP = "Lists"

// ---------------------------------------------------------------- root

private sealed class Screen {
    data object Main : Screen()
    data class Editor(val item: JSONObject?) : Screen()
    data object Settings : Screen()

    /** The front door, shown to a phone ErisDB has not let this app in from. */
    data object SignIn : Screen()
}

@Composable
fun ListsApp() {
    val ctx = LocalContext.current
    val store = remember { Store(ctx) }
    val gate = remember { SyncGate() }
    val scope = rememberCoroutineScope()
    // ErisAuth on this phone: the way in.
    val erisAuth = remember { ErisAuthSignIn(ctx) }

    // Where this app's ErisDB is, once ErisAuth has let it in.
    var server by remember { mutableStateOf(store.credentials.server) }
    var signIn by remember { mutableStateOf<SignIn>(if (erisAuth.installed) SignIn.Ready else SignIn.NoErisAuth) }
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
    var selected by remember { mutableStateOf<String?>(null) }
    var grid by remember { mutableStateOf(store.grid) }
    var linkPreviews by remember { mutableStateOf(store.linkPreviews) }
    // A just-created, still-empty list: it exists only while selected.
    var draftList by remember { mutableStateOf<String?>(null) }
    // The front door is for a phone that has never been let in. One that
    // has opens straight onto its lists.
    var screen by remember { mutableStateOf<Screen>(if (server != null) Screen.Main else Screen.SignIn) }
    var syncTick by remember { mutableStateOf(0) }
    var connectTick by remember { mutableStateOf(0) }

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
     * Ask ErisAuth on this phone to let this app in. The address its
     * approval names is kept beside the key before anything is dialed.
     */
    fun askErisAuth() {
        scope.launch {
            val app = withContext(Dispatchers.IO) { ErisDB.endpointId(store.credentials.key()) }
                ?: return@launch run { signIn = SignIn.Failed("this app's key cannot be read") }
            signIn = SignIn.Asking
            erisAuth.ask(Ask(CLIENT, MANIFEST, app)) { answer ->
                if (answer == null) return@ask run { signIn = SignIn.NoErisAuth }
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

    /** Take back the request ErisAuth is asking the person about. */
    fun cancelSignIn() {
        erisAuth.stop()
        signIn = SignIn.Ready
    }

    /** Durable-first mutation: commit to the outbox, show it, then sync. */
    fun mutate(op: JSONObject) {
        store.enqueue(op)
        outbox = store.ops()
        syncTick += 1
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

            if (server != syncingServer) return@withContext
            val updated = if (error == null) Snapshot(working, checkNotNull(cursor)) else null
            val diskError = updated?.let {
                runCatching { store.saveSnapshot(syncingServer, it) }.exceptionOrNull()
            }

            withContext(Dispatchers.Main) main@{
                if (server != syncingServer) return@main
                outbox = store.ops()
                aliases = store.aliases()
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
    // ErisAuth straight away.
    LaunchedEffect(Unit) {
        if (server != null) connect() else if (erisAuth.installed) askErisAuth()
        while (true) {
            delay(10_000)
            if (connected) sync() else if (server != null) connect()
        }
    }
    LaunchedEffect(syncTick) { if (syncTick > 0 && connected) sync() }

    // Back from installing ErisAuth: ask it.
    LifecycleResumeEffect(Unit) {
        if (server == null && signIn is SignIn.NoErisAuth && erisAuth.installed) askErisAuth()
        onPauseOrDispose { }
    }

    val shown = remember(snapshot, outbox, aliases) { applyPending(snapshot?.items?.values.orEmpty(), outbox, aliases) }
    val lists = remember(shown, draftList) {
        (shown.map { it.getJSONObject("body").getString("list") } +
            listOfNotNull(draftList)).distinct().sorted()
    }

    // The front door has nothing behind it: back from there leaves the app.
    BackHandler(enabled = screen != Screen.Main && screen != Screen.SignIn) { screen = Screen.Main }
    DisposableEffect(Unit) { onDispose { erisAuth.stop() } }

    // Sub-screens slide in from the right; going back slides them out.
    fun depth(s: Screen) = if (s is Screen.Main || s is Screen.SignIn) 0 else 1
    CompositionLocalProvider(LocalLinkPreviews provides linkPreviews) {
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
        is Screen.Main -> MainScreen(
            items = shown,
            lists = lists,
            selected = selected,
            haveData = snapshot != null || outbox.isNotEmpty(),
            grants = grants,
            statusLine = if (status == "synced") null else status,
            grid = grid,
            onGrid = { grid = it; store.grid = it },
            onSelect = { pick ->
                // A draft list vanishes the moment you look away, unless
                // an entry landed in it meanwhile.
                if (draftList != null && pick != draftList &&
                    shown.none { it.getJSONObject("body").getString("list") == draftList }
                ) draftList = null
                selected = pick
            },
            onNewList = { name -> draftList = name; selected = name },
            onOpen = { screen = Screen.Editor(it) },
            onAdd = { screen = Screen.Editor(null) },
            onSettings = { screen = Screen.Settings },
            onDelete = { item ->
                mutate(JSONObject().put("op", "delete").put("id", item.getString("id")))
            },
        )
        is Screen.Editor -> EditorScreen(
            item = s.item,
            defaultList = selected ?: "",
            lists = lists,
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
                    mutate(
                        JSONObject()
                            .put("op", "update")
                            .put("id", s.item.getString("id"))
                            .put("revision", s.item.optLong("revision"))
                            .put("body", body)
                    )
                }
                screen = Screen.Main
            },
            onBack = { screen = Screen.Main },
        )
        is Screen.Settings -> SettingsScreen(
            app = APP,
            server = server,
            signIn = signIn,
            grants = grants,
            linkPreviews = linkPreviews,
            onLinkPreviews = { linkPreviews = it; store.linkPreviews = it },
            onSignIn = ::askErisAuth,
            onCancel = ::cancelSignIn,
            onBack = { screen = Screen.Main },
        )
        is Screen.SignIn -> SignInScreen(
            app = APP,
            state = signIn,
            asked = MANIFEST.map(::describeGrant),
            onAsk = ::askErisAuth,
            onCancel = ::cancelSignIn,
        )
        }
    }
    }

    // Dial again whenever ErisAuth lets this app in anew — a new approval is
    // a command, not a condition.
    LaunchedEffect(connectTick) {
        if (connectTick > 0 && server != null && !connecting) connect()
    }
}
