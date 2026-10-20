package dev.erisdb.tasks

import dev.erisdb.android.*

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
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * Settings: what this app was granted, which ErisDB it is signed in to, and
 * signing in again — to ask for what the person did not give the first
 * time, or to reach a new ErisDB.
 */
@Composable
fun SettingsScreen(
    app: String,
    server: String?,
    signIn: SignIn,
    grants: Grants,
    onSignIn: () -> Unit,
    onCancel: () -> Unit,
    onBack: () -> Unit,
) {
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
                "ErisDB",
                style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.padding(top = 12.dp),
            )
            server?.let(::serverId)?.let {
                Text("Signed in to the ErisDB at ${it.take(12)}…", style = MaterialTheme.typography.bodyLarge)
            }
            SignInPanel(app = app, state = signIn, signedIn = server != null, onAsk = onSignIn, onCancel = onCancel)
        }
    }
}

/**
 * What this app was actually granted, next to what it asked for.
 *
 * The person answers in ErisAuth and may approve less than the whole
 * manifest, which is a legitimate answer and not an error — so the app
 * says which of its parts are switched off and why, rather than leaving
 * the user to wonder where the Add button went.
 */
@Composable
private fun Permissions(grants: Grants) {
    val missing = withheld(grants, MANIFEST)
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
    if (missing.isNotEmpty()) {
        Text(
            "Sign in again to ask for the rest; you approve it in ErisAuth.",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
