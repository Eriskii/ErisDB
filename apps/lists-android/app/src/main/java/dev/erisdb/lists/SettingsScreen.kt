package dev.erisdb.lists

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * Connection settings: which erisdb this phone is paired with, the three
 * ways to pair it with another, and whether entry links may be fetched
 * for preview images.
 *
 * The token is write access to the store, so it is treated like the
 * password it is — masked until asked for, and the window is marked
 * secure while this screen is up, which keeps it out of screenshots and
 * out of the recents-screen thumbnail.
 */
@Composable
fun SettingsScreen(
    coreName: String,
    server: String,
    token: String,
    status: String,
    connecting: Boolean,
    grants: Grants,
    linkPreviews: Boolean,
    onLinkPreviews: (Boolean) -> Unit,
    onTicket: (String) -> Unit,
    onConnect: (String, String) -> Unit,
    onBack: () -> Unit,
) {
    SecureWindow()

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "back")
            }
            Text("Settings", style = MaterialTheme.typography.titleMedium)
        }

        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text("Permissions", style = MaterialTheme.typography.labelLarge)
            Permissions(grants)

            Text(
                "Connection",
                style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.padding(top = 12.dp),
            )
            PairingPanel(
                coreName = coreName,
                server = server,
                token = token,
                status = status,
                connecting = connecting,
                onTicket = onTicket,
                onManual = onConnect,
            )

            Text("Cards", style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.padding(top = 16.dp))
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Fetch link previews", style = MaterialTheme.typography.bodyLarge)
                    Text(
                        "Loads each entry's link to find its preview image. " +
                            "The site learns this device is looking.",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Switch(checked = linkPreviews, onCheckedChange = onLinkPreviews)
            }
        }
    }
}

/**
 * What this pairing was actually granted, next to what it asked for.
 *
 * A human answers the pairing prompt and may approve less than the whole
 * manifest, which is a legitimate answer and not an error — so the app
 * says which of its parts are switched off and why, rather than leaving
 * the user to wonder where the Add button went.
 */
@Composable
private fun Permissions(grants: Grants) {
    if (grants.held == null) {
        Text(
            "not asked yet — connect and this app reads back what it was granted",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        return
    }
    for (grant in MANIFEST) {
        val ok = grants.can(grant)
        Text(
            (if (ok) "✓ " else "✗ ") + describeGrant(grant),
            style = MaterialTheme.typography.bodyMedium,
            color = if (ok) MaterialTheme.colorScheme.onSurface
                    else MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    if (withheld(grants, MANIFEST).isNotEmpty()) {
        Text(
            "Pair again to ask for the rest — approving is a keystroke on the core.",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
