package dev.erisdb.tasks

import dev.erisdb.android.*

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.Repeat
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.json.JSONObject

/**
 * The task list: open tasks in reading order, then a collapsed "Done"
 * section. The checkbox completes by the recurrence rule — a repeating
 * task rolls its due date forward instead of closing — and tapping the
 * row opens the editor.
 *
 * What is on screen is what `grants` allows. Without `tasks:create` there
 * is no Add; without `tasks:update` the checkboxes do not take a tap.
 * Hiding beats showing a control that earns a 403.
 */
@Composable
fun TasksScreen(
    items: List<JSONObject>,
    nowMs: Long,
    haveData: Boolean,
    grants: Grants,
    statusLine: String?,
    onToggle: (JSONObject) -> Unit,
    onOpen: (JSONObject) -> Unit,
    onAdd: () -> Unit,
    onSettings: () -> Unit,
) {
    var showDone by remember { mutableStateOf(false) }

    val open = items.filter { !it.getJSONObject("body").optBoolean("done") }
    val done = items.filter { it.getJSONObject("body").optBoolean("done") }

    Box(Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize()) {
            Row(
                Modifier.fillMaxWidth().padding(start = 20.dp, end = 8.dp, top = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    "Tasks",
                    style = MaterialTheme.typography.headlineSmall,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.weight(1f),
                )
                IconButton(onClick = onSettings) {
                    Icon(Icons.Default.Settings, contentDescription = "settings")
                }
            }

            statusLine?.let {
                Text(it, Modifier.padding(horizontal = 20.dp, vertical = 2.dp),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant)
            }

            if (grants.readOnly) {
                Text(
                    "read-only · this pairing was granted ${describeGrant("$FACET:read")} " +
                        "and nothing more",
                    Modifier.padding(horizontal = 20.dp, vertical = 2.dp),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }

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
                // A plain scroll column kept 1dp taller than the viewport:
                // the stretch overscroll engages even on short lists.
                BoxWithConstraints(Modifier.weight(1f).fillMaxWidth()) {
                    val minHeight = maxHeight + 1.dp
                    Column(
                        Modifier
                            .fillMaxWidth()
                            .verticalScroll(rememberScrollState())
                            .heightIn(min = minHeight),
                    ) {
                        open.forEach { item ->
                            TaskRow(item, nowMs, grants.mayUpdate, { onToggle(item) }) {
                                onOpen(item)
                            }
                        }
                        if (open.isEmpty()) {
                            Text(
                                if (done.isEmpty()) "nothing to do yet" else "all clear",
                                Modifier.fillMaxWidth().padding(32.dp),
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }

                        if (done.isNotEmpty()) {
                            HorizontalDivider(Modifier.padding(vertical = 4.dp))
                            Row(
                                Modifier
                                    .fillMaxWidth()
                                    .clickable { showDone = !showDone }
                                    .padding(horizontal = 20.dp, vertical = 12.dp),
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Text(
                                    "Done · ${done.size}",
                                    style = MaterialTheme.typography.labelLarge,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                    modifier = Modifier.weight(1f),
                                )
                                Icon(
                                    if (showDone) Icons.Default.ExpandLess else Icons.Default.ExpandMore,
                                    contentDescription = if (showDone) "hide done" else "show done",
                                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                            AnimatedVisibility(
                                visible = showDone,
                                enter = fadeIn() + expandVertically(),
                                exit = fadeOut() + shrinkVertically(),
                            ) {
                                Column {
                                    done.forEach { item ->
                                        TaskRow(item, nowMs, grants.mayUpdate, { onToggle(item) }) {
                                onOpen(item)
                            }
                                    }
                                }
                            }
                        }

                        Spacer(Modifier.height(88.dp)) // clears the FAB
                    }
                }
            }
        }

        if (grants.mayCreate) {
            FloatingActionButton(
                onClick = onAdd,
                modifier = Modifier.align(Alignment.BottomEnd).padding(20.dp),
            ) { Icon(Icons.Default.Add, contentDescription = "add task") }
        }
    }
}

@Composable
private fun TaskRow(
    item: JSONObject,
    nowMs: Long,
    mayComplete: Boolean,
    onToggle: () -> Unit,
    onOpen: () -> Unit,
) {
    val b = item.getJSONObject("body")
    val pending = item.getString("id").startsWith(PENDING)
    val isDone = b.optBoolean("done")
    val due = dueOf(b)
    val repeat = repeatOf(b)
    val overdue = isOverdue(b, nowMs)

    Row(
        Modifier.fillMaxWidth().clickable(onClick = onOpen).padding(end = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        // Completing a task is an update. Without that grant the box shows
        // the state and refuses the tap.
        Checkbox(
            checked = isDone,
            onCheckedChange = { onToggle() },
            enabled = mayComplete,
        )
        Column(Modifier.weight(1f).padding(vertical = 10.dp)) {
            Text(
                b.optString("title") + if (pending) "  ⋯" else "",
                style = MaterialTheme.typography.bodyLarge,
                color = if (isDone) MaterialTheme.colorScheme.onSurfaceVariant
                        else MaterialTheme.colorScheme.onSurface,
                textDecoration = if (isDone) TextDecoration.LineThrough else null,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
            if (due != null || repeat != null) {
                Row(
                    Modifier.padding(top = 3.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    due?.let {
                        Text(
                            formatDue(it, nowMs) + if (overdue) " · overdue" else "",
                            style = MaterialTheme.typography.labelMedium,
                            color = if (overdue) MaterialTheme.colorScheme.error
                                    else MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    repeat?.let { (n, unit) -> RepeatBadge(describe(n, unit)) }
                }
            }
        }
    }
}

/** The repeat rule, worn where the due date can be read beside it. */
@Composable
private fun RepeatBadge(label: String) {
    Surface(
        shape = RoundedCornerShape(8.dp),
        color = MaterialTheme.colorScheme.secondaryContainer,
    ) {
        Row(
            Modifier.padding(horizontal = 6.dp, vertical = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(3.dp),
        ) {
            Icon(
                Icons.Default.Repeat,
                contentDescription = null,
                modifier = Modifier.height(13.dp),
                tint = MaterialTheme.colorScheme.onSecondaryContainer,
            )
            Text(
                label,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSecondaryContainer,
            )
        }
    }
}
