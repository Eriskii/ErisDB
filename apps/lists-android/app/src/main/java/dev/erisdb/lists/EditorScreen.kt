package dev.erisdb.lists

import dev.erisdb.android.*

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ArrowDropDown
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import org.json.JSONArray
import org.json.JSONObject

/**
 * Fullscreen add/edit. `item == null` adds a new entry into `defaultList`;
 * otherwise it edits the item. `onSave` receives the finished body.
 * An `image` attribute (added like any other attribute) sets the card
 * picture, overriding the link's site image.
 *
 * Adding and editing are separate permissions, so this screen can be a
 * reader: the fields still show an entry and Save is gone.
 */
@Composable
fun EditorScreen(
    item: JSONObject?,
    defaultList: String,
    lists: List<String>,
    grants: Grants,
    onSave: (JSONObject) -> Unit,
    onBack: () -> Unit,
) {
    val b = item?.getJSONObject("body")
    val pending = item?.getString("id")?.startsWith(PENDING) == true
    val maySave = if (item == null) grants.mayCreate else grants.mayUpdate
    var list by remember { mutableStateOf(b?.getString("list") ?: defaultList) }
    var name by remember { mutableStateOf(b?.getString("name") ?: "") }
    var desc by remember { mutableStateOf(b?.optString("description") ?: "") }
    var link by remember { mutableStateOf(b?.optString("link") ?: "") }
    val attrRows = remember {
        mutableStateListOf<Pair<String, String>>().apply {
            b?.optJSONObject("attributes")?.let { a ->
                a.keys().forEach { k -> add(k to a.get(k).toString()) }
            }
        }
    }
    var listMenu by remember { mutableStateOf(false) }

    fun buildBody(): JSONObject {
        val body = JSONObject().put("list", list.trim()).put("name", name.trim())
        if (desc.isNotBlank()) body.put("description", desc.trim())
        if (link.isNotBlank()) body.put("link", link.trim())
        val parsed = JSONObject()
        attrRows.forEach { (k, v) ->
            val key = k.trim()
            val raw = v.trim()
            if (key.isEmpty() || raw.isEmpty()) return@forEach
            val value: Any = raw.toLongOrNull() ?: raw.toDoubleOrNull()
                ?: when (raw) {
                    "true" -> true; "false" -> false
                    else -> if (raw.startsWith("[")) {
                        try { JSONArray(raw) } catch (_: Exception) { raw }
                    } else raw
                }
            parsed.put(key, value)
        }
        if (parsed.length() > 0) body.put("attributes", parsed)
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
                if (item == null) "New entry" else if (maySave) "Edit entry" else "Entry",
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.weight(1f),
            )
            if (maySave) {
                TextButton(
                    onClick = { onSave(buildBody()) },
                    enabled = !pending && list.isNotBlank() && name.isNotBlank(),
                ) { Text("Save") }
            }
        }
        if (!maySave) {
            Text(
                "this pairing was not granted ${describeGrant("$FACET:update")}",
                Modifier.padding(horizontal = 16.dp),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.error)
        }
        if (pending) {
            Text("still syncing — edit after it lands",
                Modifier.padding(horizontal = 16.dp),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant)
        }

        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Box {
                OutlinedTextField(
                    list, { list = it },
                    Modifier.fillMaxWidth(),
                    label = { Text("list") },
                    singleLine = true,
                    trailingIcon = {
                        IconButton(onClick = { listMenu = true }) {
                            Icon(Icons.Default.ArrowDropDown, contentDescription = "pick list")
                        }
                    },
                )
                DropdownMenu(expanded = listMenu, onDismissRequest = { listMenu = false }) {
                    lists.forEach { n ->
                        DropdownMenuItem(text = { Text(n) }, onClick = { list = n; listMenu = false })
                    }
                }
            }
            OutlinedTextField(name, { name = it }, Modifier.fillMaxWidth(),
                label = { Text("name") }, singleLine = true)
            OutlinedTextField(desc, { desc = it }, Modifier.fillMaxWidth(),
                label = { Text("description (markdown)") }, minLines = 4)
            OutlinedTextField(link, { link = it }, Modifier.fillMaxWidth(),
                label = { Text("link") }, singleLine = true)

            Text("Attributes", style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.padding(top = 6.dp))
            attrRows.forEachIndexed { i, (k, v) ->
                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    OutlinedTextField(k, { attrRows[i] = it to v }, Modifier.weight(0.4f),
                        label = { Text("key") }, singleLine = true)
                    OutlinedTextField(v, { attrRows[i] = k to it }, Modifier.weight(0.6f),
                        label = { Text("value") }, singleLine = true)
                    IconButton(onClick = { attrRows.removeAt(i) }) {
                        Icon(Icons.Default.Close, contentDescription = "remove attribute")
                    }
                }
            }
            TextButton(onClick = { attrRows.add("" to "") }) {
                Icon(Icons.Default.Add, contentDescription = null)
                Text("Add attribute", Modifier.padding(start = 4.dp))
            }

            if (item != null) {
                Text(
                    "added ${item.optString("created_at").take(10)} · modified ${item.optString("updated_at").take(10)}",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}
