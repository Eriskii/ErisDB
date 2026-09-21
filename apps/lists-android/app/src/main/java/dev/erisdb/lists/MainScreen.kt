package dev.erisdb.lists

import dev.erisdb.android.*

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.lazy.staggeredgrid.LazyVerticalStaggeredGrid
import androidx.compose.foundation.lazy.staggeredgrid.StaggeredGridCells
import androidx.compose.foundation.lazy.staggeredgrid.StaggeredGridItemSpan
import androidx.compose.foundation.lazy.staggeredgrid.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.automirrored.filled.Notes
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.GridView
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.SelectAll
import androidx.compose.material.icons.filled.Title
import androidx.compose.material.icons.filled.ViewAgenda
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupPositionProvider
import androidx.compose.ui.window.PopupProperties
import kotlinx.coroutines.delay
import org.json.JSONObject

private enum class SearchMode(val label: String, val icon: ImageVector) {
    All("searching all fields", Icons.Default.SelectAll),
    Title("searching titles only", Icons.Default.Title),
    Body("searching descriptions only", Icons.AutoMirrored.Filled.Notes),
}

/**
 * Cards over a bottom bar: dots, search pill, add — list picker above add.
 *
 * What is on screen is what `grants` allows. Without `lists:create` there
 * is no Add; without `lists:delete` the press-hold menu offers copying and
 * nothing else. Hiding beats showing a control that earns a 403.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun MainScreen(
    items: List<JSONObject>,
    lists: List<String>,
    selected: String?,
    haveData: Boolean,
    grants: Grants,
    statusLine: String?,
    grid: Boolean,
    onGrid: (Boolean) -> Unit,
    onSelect: (String?) -> Unit,
    onNewList: (String) -> Unit,
    onOpen: (JSONObject) -> Unit,
    onAdd: () -> Unit,
    onSettings: () -> Unit,
    onDelete: (JSONObject) -> Unit,
) {
    var listMenu by remember { mutableStateOf(false) }
    var dotsMenu by remember { mutableStateOf(false) }
    var modeMenu by remember { mutableStateOf(false) }
    var newListDialog by remember { mutableStateOf(false) }
    var longPressed by remember { mutableStateOf<JSONObject?>(null) }
    var confirmDelete by remember { mutableStateOf<JSONObject?>(null) }
    var query by remember { mutableStateOf("") }
    var mode by remember { mutableStateOf(SearchMode.All) }
    // Text and visibility are separate so the pill fades out with its
    // text intact instead of collapsing around vanishing characters.
    var noticeText by remember { mutableStateOf("") }
    var noticeVisible by remember { mutableStateOf(false) }
    var noticeStamp by remember { mutableStateOf(0) }

    fun announce(text: String) { noticeText = text; noticeVisible = true; noticeStamp += 1 }
    LaunchedEffect(noticeStamp) {
        if (noticeVisible) { delay(1600); noticeVisible = false }
    }

    fun matches(item: JSONObject): Boolean {
        if (query.isBlank()) return true
        val q = query.trim().lowercase()
        val b = item.getJSONObject("body")
        return when (mode) {
            SearchMode.Title -> b.getString("name").lowercase().contains(q)
            SearchMode.Body -> b.optString("description").lowercase().contains(q)
            SearchMode.All -> {
                val attrs = b.optJSONObject("attributes")?.let { a ->
                    a.keys().asSequence().joinToString(" ") { k -> "$k ${a.get(k)}" }
                } ?: ""
                listOf(
                    b.getString("name"), b.optString("description"),
                    b.optString("link"), attrs,
                ).any { it.lowercase().contains(q) }
            }
        }
    }

    val inList = items
        .filter { selected == null || it.getJSONObject("body").getString("list") == selected }
        .sortedBy { it.getJSONObject("body").getString("name").lowercase() }
    // Matches animate to their new grid slots via animateItem as the
    // query narrows, instead of snapping.
    val shown = inList.filter(::matches)

    Box(Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize()) {
            statusLine?.let {
                Text(it, Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant)
            }

            if (grants.readOnly) {
                Text(
                    "read-only · this pairing was granted ${describeGrant("$FACET:read")} " +
                        "and nothing more",
                    Modifier.padding(horizontal = 24.dp, vertical = 2.dp),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }

            // ------------------------------------------------ cards
            if (!haveData) {
                Column(
                    Modifier.fillMaxSize(),
                    verticalArrangement = Arrangement.Center,
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    CircularProgressIndicator()
                    Text("syncing over iroh…", Modifier.padding(top = 12.dp),
                        style = MaterialTheme.typography.bodySmall)
                }
            } else {
                // Keep-style mosaic when grid is on: two staggered columns,
                // each card as tall as its content demands. A one-column
                // staggered grid is the plain full-width list.
                LazyVerticalStaggeredGrid(
                    columns = StaggeredGridCells.Fixed(if (grid) 2 else 1),
                    modifier = Modifier.weight(1f).fillMaxWidth(),
                    contentPadding = PaddingValues(
                        start = 12.dp, end = 12.dp, top = 2.dp, bottom = 156.dp, // clears the bottom bar
                    ),
                    verticalItemSpacing = 12.dp,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    items(shown, key = { it.getString("id") }) { item ->
                        EntryCard(
                            item = item,
                            showList = selected == null,
                            onOpen = { onOpen(item) },
                            onLongPress = { longPressed = item },
                            descriptionLines = if (grid) 8 else 3,
                            modifier = Modifier.animateItem(),
                        )
                    }
                    if (shown.isEmpty()) {
                        item(span = StaggeredGridItemSpan.FullLine) {
                            Text(
                                if (query.isBlank()) "nothing here yet" else "no matches",
                                Modifier.fillMaxWidth().padding(32.dp),
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
            }
        }

        // ------------------------------------------------ bottom bar:
        // [dots] [search pill] [add], list picker floating above add —
        // everything within thumb reach.
        Column(
            Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .padding(horizontal = 16.dp)
                .padding(bottom = 16.dp),
            horizontalAlignment = Alignment.End,
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Box {
                FilledTonalIconButton(
                    onClick = { listMenu = true },
                    shape = MaterialTheme.shapes.large, // matches the FAB
                    modifier = Modifier.size(56.dp),
                ) {
                    Icon(Icons.AutoMirrored.Filled.List, contentDescription = "choose list")
                }
                UpMenu(
                    expanded = listMenu,
                    onDismissRequest = { listMenu = false },
                    modifier = Modifier.fillMaxWidth(0.8f),
                ) {
                    ListMenuItem("All", items.size, selected == null) { onSelect(null); listMenu = false }
                    lists.forEach { name ->
                        val n = items.count { it.getJSONObject("body").getString("list") == name }
                        ListMenuItem(name, n, selected == name) { onSelect(name); listMenu = false }
                    }
                    HorizontalDivider()
                    DropdownMenuItem(
                        text = { Text("New list…", style = MaterialTheme.typography.titleMedium) },
                        onClick = { listMenu = false; newListDialog = true },
                    )
                }
            }

            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Box {
                    FilledTonalIconButton(
                        onClick = { dotsMenu = true },
                        shape = MaterialTheme.shapes.large, // matches the FAB
                        modifier = Modifier.size(56.dp),
                    ) {
                        Icon(Icons.Default.MoreVert, contentDescription = "menu")
                    }
                    UpMenu(expanded = dotsMenu, onDismissRequest = { dotsMenu = false }) {
                        DropdownMenuItem(
                            text = { Text(if (grid) "List layout" else "Grid layout") },
                            leadingIcon = {
                                Icon(
                                    if (grid) Icons.Default.ViewAgenda else Icons.Default.GridView,
                                    contentDescription = null,
                                )
                            },
                            onClick = { dotsMenu = false; onGrid(!grid) },
                        )
                        DropdownMenuItem(
                            text = { Text("Settings") },
                            onClick = { dotsMenu = false; onSettings() },
                        )
                    }
                }

                // The search pill: a stretched FAB shape floating over the
                // cards, search/clear and mode toggle both on its left.
                Surface(
                    modifier = Modifier.weight(1f),
                    shape = MaterialTheme.shapes.large, // matches the FAB
                    shadowElevation = 6.dp,
                ) {
                    OutlinedTextField(
                        query, { query = it },
                        modifier = Modifier.fillMaxWidth(),
                        placeholder = {
                            Text("Search ${selected ?: "All"}", maxLines = 1, overflow = TextOverflow.Ellipsis)
                        },
                        leadingIcon = {
                            if (query.isEmpty()) {
                                Icon(Icons.Default.Search, contentDescription = null)
                            } else {
                                IconButton(onClick = { query = "" }) {
                                    Icon(Icons.Default.Close, contentDescription = "clear search")
                                }
                            }
                        },
                        trailingIcon = {
                            Box {
                                // Tap cycles the mode; press-hold picks from a list.
                                Box(
                                    Modifier
                                        .clip(androidx.compose.foundation.shape.CircleShape)
                                        .combinedClickable(
                                            onClick = {
                                                mode = SearchMode.entries[
                                                    (mode.ordinal + 1) % SearchMode.entries.size]
                                                announce(mode.label)
                                            },
                                            onLongClick = { modeMenu = true },
                                        )
                                        .padding(8.dp),
                                ) {
                                    Icon(mode.icon, contentDescription = mode.label)
                                }
                                UpMenu(expanded = modeMenu, onDismissRequest = { modeMenu = false }) {
                                    SearchMode.entries.forEach { m ->
                                        DropdownMenuItem(
                                            text = { Text(m.label) },
                                            leadingIcon = { Icon(m.icon, contentDescription = null) },
                                            onClick = { mode = m; announce(m.label); modeMenu = false },
                                        )
                                    }
                                }
                            }
                        },
                        singleLine = true,
                        shape = MaterialTheme.shapes.large,
                        colors = TextFieldDefaults.colors(
                            unfocusedIndicatorColor = androidx.compose.ui.graphics.Color.Transparent,
                            focusedIndicatorColor = androidx.compose.ui.graphics.Color.Transparent,
                        ),
                    )
                }

                if (grants.mayCreate) {
                    FloatingActionButton(onClick = onAdd) {
                        Icon(Icons.Default.Add, contentDescription = "add entry")
                    }
                }
            }
        }

        // The transient "what mode am I in" pill: an overlay, so it never
        // displaces content, and its text survives the fade-out.
        AnimatedVisibility(
            visible = noticeVisible,
            enter = fadeIn(),
            exit = fadeOut(),
            modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 152.dp),
        ) {
            Surface(
                shape = androidx.compose.foundation.shape.CircleShape,
                color = MaterialTheme.colorScheme.secondaryContainer,
                shadowElevation = 3.dp,
            ) {
                Text(
                    noticeText,
                    Modifier.padding(horizontal = 14.dp, vertical = 6.dp),
                    style = MaterialTheme.typography.labelMedium,
                )
            }
        }
    }

    if (newListDialog) {
        var name by remember { mutableStateOf("") }
        AlertDialog(
            onDismissRequest = { newListDialog = false },
            title = { Text("New list") },
            text = {
                OutlinedTextField(name, { name = it }, label = { Text("name") }, singleLine = true)
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        if (name.isNotBlank()) { onNewList(name.trim()); newListDialog = false }
                    },
                ) { Text("Create") }
            },
            dismissButton = { TextButton({ newListDialog = false }) { Text("Cancel") } },
        )
    }

    longPressed?.let { item ->
        EntryActions(
            item = item,
            mayDelete = grants.mayDelete,
            onDismiss = { longPressed = null },
            onDelete = { longPressed = null; confirmDelete = item },
        )
    }

    confirmDelete?.let { item ->
        AlertDialog(
            onDismissRequest = { confirmDelete = null },
            title = { Text("Delete \"${item.getJSONObject("body").getString("name")}\"?") },
            text = { Text("This removes the entry from the core. Its history is kept.") },
            confirmButton = {
                TextButton({ onDelete(item); confirmDelete = null }) {
                    Text("Delete", color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = { TextButton({ confirmDelete = null }) { Text("Cancel") } },
        )
    }
}

/** A menu that hugs its anchor from above: material3's DropdownMenu
 * keeps a 48dp margin from the window edge, which strands it well
 * above anchors this close to the bottom of the screen. This popup
 * pins its bottom 4dp over the anchor's top instead, in the app's
 * rounded shape language. */
@Composable
private fun UpMenu(
    expanded: Boolean,
    onDismissRequest: () -> Unit,
    modifier: Modifier = Modifier,
    content: @Composable ColumnScope.() -> Unit,
) {
    if (!expanded) return
    val gap = with(LocalDensity.current) { 4.dp.roundToPx() }
    Popup(
        popupPositionProvider = remember(gap) {
            object : PopupPositionProvider {
                override fun calculatePosition(
                    anchorBounds: IntRect,
                    windowSize: IntSize,
                    layoutDirection: LayoutDirection,
                    popupContentSize: IntSize,
                ): IntOffset = IntOffset(
                    anchorBounds.left
                        .coerceAtMost(windowSize.width - popupContentSize.width)
                        .coerceAtLeast(0),
                    (anchorBounds.top - popupContentSize.height - gap)
                        .coerceAtLeast(0),
                )
            }
        },
        onDismissRequest = onDismissRequest,
        properties = PopupProperties(focusable = true),
    ) {
        // Grow-and-fade in from the anchor's corner, like a menu does.
        var shown by remember { mutableStateOf(false) }
        LaunchedEffect(Unit) { shown = true }
        val scale by animateFloatAsState(if (shown) 1f else 0.85f, label = "menuScale")
        val alpha by animateFloatAsState(if (shown) 1f else 0f, label = "menuAlpha")
        Surface(
            modifier = Modifier.graphicsLayer {
                this.alpha = alpha
                scaleX = scale
                scaleY = scale
                transformOrigin = TransformOrigin(0.1f, 1f)
            },
            shape = MaterialTheme.shapes.large, // matches the bar's buttons
            color = MaterialTheme.colorScheme.surfaceContainer,
            shadowElevation = 6.dp,
        ) {
            Column(
                modifier
                    .width(IntrinsicSize.Max)
                    .heightIn(max = 420.dp)
                    .verticalScroll(rememberScrollState())
                    .padding(vertical = 8.dp),
                content = content,
            )
        }
    }
}

@Composable
private fun ListMenuItem(name: String, count: Int, active: Boolean, onClick: () -> Unit) {
    DropdownMenuItem(
        text = {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    name,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = if (active) FontWeight.Bold else FontWeight.Normal,
                    modifier = Modifier.weight(1f),
                )
                Text("$count", style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        },
        onClick = onClick,
    )
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun EntryCard(
    item: JSONObject,
    showList: Boolean,
    onOpen: () -> Unit,
    onLongPress: () -> Unit,
    descriptionLines: Int,
    modifier: Modifier = Modifier,
) {
    val b = item.getJSONObject("body")
    val pending = item.getString("id").startsWith(PENDING)
    Card(
        modifier
            .fillMaxWidth()
            .combinedClickable(onClick = onOpen, onLongClick = onLongPress),
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
    ) {
        Column(Modifier.padding(12.dp)) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    b.getString("name") + if (pending) "  ⋯" else "",
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    color = MaterialTheme.colorScheme.onSurface,
                    modifier = Modifier.weight(1f),
                )
                if (showList) {
                    Text(b.getString("list"), style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            b.optString("description").ifBlank { null }?.let {
                Text(
                    markdownToAnnotated(it),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = descriptionLines,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            ItemImage(b)
        }
    }
}

/** The press-hold menu: copy things, or delete (confirmed by the caller).
 * Copying is local, so it is always there; deleting is a permission. */
@Composable
private fun EntryActions(
    item: JSONObject,
    mayDelete: Boolean,
    onDismiss: () -> Unit,
    onDelete: () -> Unit,
) {
    val b = item.getJSONObject("body")
    val clipboard = LocalClipboardManager.current
    val name = b.getString("name")
    val desc = b.optString("description").ifBlank { null }
    val link = b.optString("link").ifBlank { null }
    val image = explicitImage(b)

    fun copy(s: String) { clipboard.setText(AnnotatedString(s)); onDismiss() }

    Dialog(onDismissRequest = onDismiss) {
        Card {
            Column(Modifier.padding(vertical = 8.dp)) {
                Text(name, Modifier.padding(horizontal = 20.dp, vertical = 8.dp),
                    style = MaterialTheme.typography.titleMedium)
                HorizontalDivider()
                ActionRow("Copy title") { copy(name) }
                desc?.let { ActionRow("Copy description") { copy(it) } }
                desc?.let { ActionRow("Copy title + description") { copy("$name\n\n$it") } }
                link?.let { ActionRow("Copy link") { copy(it) } }
                image?.let { ActionRow("Copy image URL") { copy(it) } }
                if (mayDelete) {
                    HorizontalDivider()
                    ActionRow("Delete", danger = true, onClick = onDelete)
                }
            }
        }
    }
}

@Composable
private fun ActionRow(label: String, danger: Boolean = false, onClick: () -> Unit) {
    TextButton(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Text(
            label,
            Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 2.dp),
            color = if (danger) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurface,
        )
    }
}
