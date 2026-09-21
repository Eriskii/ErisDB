package dev.erisdb.tasks

import dev.erisdb.android.*

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Event
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DatePicker
import androidx.compose.material3.DatePickerDialog
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TimePicker
import androidx.compose.material3.rememberDatePickerState
import androidx.compose.material3.rememberTimePickerState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import org.json.JSONObject
import java.time.Instant
import java.time.LocalDate
import java.time.LocalDateTime
import java.time.LocalTime
import java.time.ZoneId
import java.time.ZoneOffset

/**
 * Fullscreen add/edit. `item == null` adds a new task; otherwise it edits
 * one, and delete lives here too. `onSave` receives the finished body,
 * built to the strict schema: due, notes and repeat are absent rather
 * than empty when unset.
 *
 * Adding and editing are separate permissions, so this screen can be a
 * reader: the fields still show a task, Save is gone, and Delete is there
 * only with `tasks:delete`.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun EditorScreen(
    item: JSONObject?,
    grants: Grants,
    onSave: (JSONObject) -> Unit,
    onDelete: () -> Unit,
    onBack: () -> Unit,
) {
    val zone = ZoneId.systemDefault()
    val b = item?.getJSONObject("body")
    val pending = item?.getString("id")?.startsWith(PENDING) == true
    val maySave = if (item == null) grants.mayCreate else grants.mayUpdate

    var title by remember { mutableStateOf(b?.optString("title") ?: "") }
    var notes by remember { mutableStateOf(b?.optString("notes") ?: "") }
    var dueMs by remember {
        mutableStateOf(b?.let { dueOf(it) }?.toInstant()?.toEpochMilli())
    }
    val existingRepeat = b?.let { repeatOf(it) }
    var unit by remember { mutableStateOf(existingRepeat?.second) }
    var every by remember { mutableStateOf(existingRepeat?.first?.toString() ?: "1") }

    var datePicker by remember { mutableStateOf(false) }
    // The date chosen but not yet given a time — the two dialogs run in
    // sequence so a due date always carries a clock time.
    var pendingDate by remember { mutableStateOf<LocalDate?>(null) }
    var confirmDelete by remember { mutableStateOf(false) }

    fun buildBody(): JSONObject {
        val body = JSONObject()
            .put("title", title.trim())
            .put("done", b?.optBoolean("done") ?: false)
        dueMs?.let { body.put("due", Instant.ofEpochMilli(it).toString()) }
        if (notes.isNotBlank()) body.put("notes", notes.trim())
        unit?.let { u ->
            val n = every.trim().toIntOrNull()?.coerceAtLeast(1) ?: 1
            body.put("repeat", JSONObject().put("n", n).put("unit", u.wire))
        }
        return body
    }

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "back")
            }
            Text(
                if (item == null) "New task" else if (maySave) "Edit task" else "Task",
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.weight(1f),
            )
            if (maySave) {
                TextButton(
                    onClick = { onSave(buildBody()) },
                    enabled = !pending && title.isNotBlank(),
                ) { Text("Save") }
            }
        }
        if (pending) {
            Text("still syncing — edit after it lands",
                Modifier.padding(horizontal = 16.dp),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        if (!maySave) {
            Text(
                "this pairing was not granted ${describeGrant("$FACET:update")}",
                Modifier.padding(horizontal = 16.dp),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.error,
            )
        }

        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            OutlinedTextField(title, { title = it }, Modifier.fillMaxWidth(),
                label = { Text("title") }, singleLine = true)
            OutlinedTextField(notes, { notes = it }, Modifier.fillMaxWidth(),
                label = { Text("notes") }, minLines = 4)

            Text("Due", style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.padding(top = 6.dp))
            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                OutlinedButton(onClick = { datePicker = true }, modifier = Modifier.weight(1f)) {
                    Icon(Icons.Default.Event, contentDescription = null)
                    Text(
                        dueMs?.let {
                            formatDue(Instant.ofEpochMilli(it).atZone(zone).toOffsetDateTime(),
                                System.currentTimeMillis(), zone)
                        } ?: "no due date",
                        Modifier.padding(start = 8.dp),
                    )
                }
                if (dueMs != null) {
                    IconButton(onClick = { dueMs = null }) {
                        Icon(Icons.Default.Close, contentDescription = "clear due date")
                    }
                }
            }

            Text("Repeat", style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.padding(top = 6.dp))
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                FilterChip(
                    selected = unit == null,
                    onClick = { unit = null },
                    label = { Text("none") },
                )
                RepeatUnit.entries.forEach { u ->
                    FilterChip(
                        selected = unit == u,
                        onClick = { unit = u },
                        label = { Text(u.wire) },
                    )
                }
            }
            unit?.let { u ->
                Row(
                    Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    OutlinedTextField(
                        every,
                        { every = it.filter(Char::isDigit).take(3) },
                        Modifier.width(110.dp),
                        label = { Text("every") },
                        singleLine = true,
                    )
                    Text(
                        describe(every.trim().toIntOrNull()?.coerceAtLeast(1) ?: 1, u),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                if (dueMs == null) {
                    Text(
                        "a repeating task needs a due date to roll forward from",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            if (item != null) {
                Text(
                    "added ${item.optString("created_at").take(10)} · modified ${item.optString("updated_at").take(10)}",
                    Modifier.padding(top = 10.dp),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (grants.mayDelete) {
                    TextButton(onClick = { confirmDelete = true }) {
                        Text("Delete task", color = MaterialTheme.colorScheme.error)
                    }
                }
            }
        }
    }

    if (datePicker) {
        val state = rememberDatePickerState(initialSelectedDateMillis = dueMs)
        DatePickerDialog(
            onDismissRequest = { datePicker = false },
            confirmButton = {
                TextButton(
                    onClick = {
                        // The picker speaks UTC midnight; the calendar day
                        // it means is the one to hand to the clock.
                        state.selectedDateMillis?.let {
                            pendingDate = Instant.ofEpochMilli(it)
                                .atZone(ZoneOffset.UTC).toLocalDate()
                        }
                        datePicker = false
                    },
                    enabled = state.selectedDateMillis != null,
                ) { Text("Next") }
            },
            dismissButton = { TextButton({ datePicker = false }) { Text("Cancel") } },
        ) { DatePicker(state = state) }
    }

    pendingDate?.let { date ->
        val current = dueMs?.let { Instant.ofEpochMilli(it).atZone(zone).toLocalTime() }
        val state = rememberTimePickerState(
            initialHour = current?.hour ?: 9,
            initialMinute = current?.minute ?: 0,
            is24Hour = true,
        )
        AlertDialog(
            onDismissRequest = { pendingDate = null },
            title = { Text("Time") },
            text = { TimePicker(state = state) },
            confirmButton = {
                TextButton({
                    dueMs = LocalDateTime.of(date, LocalTime.of(state.hour, state.minute))
                        .atZone(zone).toInstant().toEpochMilli()
                    pendingDate = null
                }) { Text("Set") }
            },
            dismissButton = { TextButton({ pendingDate = null }) { Text("Cancel") } },
        )
    }

    if (confirmDelete) {
        AlertDialog(
            onDismissRequest = { confirmDelete = false },
            title = { Text("Delete \"${title.trim()}\"?") },
            text = { Text("This removes the task from the core. Its history is kept.") },
            confirmButton = {
                TextButton({ confirmDelete = false; onDelete() }) {
                    Text("Delete", color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = { TextButton({ confirmDelete = false }) { Text("Cancel") } },
        )
    }
}
