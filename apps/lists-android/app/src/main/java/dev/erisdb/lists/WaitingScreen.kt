package dev.erisdb.lists

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * The half of pairing that happens on somebody else's keyboard.
 *
 * Redeeming a code raises a prompt on the core naming this app and the
 * permissions it asked for, and nothing is granted until a person answers.
 * So this screen says who it is waiting for, shows exactly what was asked
 * for — the phone's copy of the prompt on the other machine — and offers
 * the one thing the user can do from here, which is stop waiting.
 */
@Composable
fun WaitingScreen(
    coreName: String,
    state: Approval,
    requested: List<String>,
    onCancel: () -> Unit,
    onBack: () -> Unit,
) {
    SecureWindow()
    val there = coreName.ifBlank { "the core" }

    Column(
        Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        when (state) {
            is Approval.Pending, is Approval.Approved, is Approval.Waiting, is Approval.Unreachable -> {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator(Modifier.size(22.dp), strokeWidth = 2.dp)
                    Text(
                        if (state is Approval.Pending) "Requesting access to $there…"
                        else "Waiting for approval on $there…",
                        style = MaterialTheme.typography.titleMedium,
                        modifier = Modifier.padding(start = 14.dp),
                    )
                }
                Text(
                    if (state is Approval.Pending) "Connecting to send this app’s permission request."
                    else "This app has asked $there for permission. Approve it there — in " +
                        "the erisdb pair terminal, or any app that can approve pairings.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (state is Approval.Waiting && state.fingerprint != null) {
                    Text("Compare ${state.fingerprint} with the terminal before approving.",
                        style = MaterialTheme.typography.titleMedium)
                }
                Asked(requested)
                if (state is Approval.Unreachable) {
                    Text(
                        "can't reach $there: ${state.error} · still trying",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
                Button(onClick = onCancel) { Text("Cancel") }
            }

            is Approval.Denied -> Ending(
                title = "$there said no",
                body = "Nothing was granted and nothing was stored. Cut another code " +
                    "with erisdb pair when you want to try again.",
                onBack = onBack,
            )

            is Approval.Over -> Ending(
                title = "That pairing code is done",
                body = state.reason + ". A code is good for a few minutes; run " +
                    "erisdb pair on $there for a fresh one.",
                onBack = onBack,
            )
        }
    }
}

/** The permissions asked for, in the words the prompt on the core uses. */
@Composable
private fun Asked(requested: List<String>) {
    Card(colors = CardDefaults.cardColors(MaterialTheme.colorScheme.surfaceVariant)) {
        Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text("Asked for", style = MaterialTheme.typography.labelLarge)
            requested.forEach {
                Text("· ${describeGrant(it)}", style = MaterialTheme.typography.bodyMedium)
            }
            Text(
                "You can approve fewer than these. This app hides what it was not given.",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun Ending(title: String, body: String, onBack: () -> Unit) {
    Text(title, style = MaterialTheme.typography.titleMedium)
    Text(
        body,
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    Row(Modifier.fillMaxWidth()) {
        TextButton(onClick = onBack) { Text("Back to pairing") }
    }
}
