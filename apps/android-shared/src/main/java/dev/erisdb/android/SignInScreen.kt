package dev.erisdb.android

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Key
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * The front door, the first thing a phone sees until the app is let in: ErisAuth on this phone
 * is the way in, and the only one. [asked] is what the app asks for, in the words its screens use.
 */
@Composable
fun SignInScreen(app: String, state: SignIn, asked: List<String>, onAsk: () -> Unit, onCancel: () -> Unit) {
    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(Icons.Filled.Key, contentDescription = null)
            Text("Sign in", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(start = 10.dp))
        }
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                "$app keeps its data in your ErisDB, and ErisAuth on this phone lets it in. You approve " +
                    "$app there, choose what it may do, and type ErisDB's passphrase.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            SignInPanel(app = app, state = state, signedIn = false, onAsk = onAsk, onCancel = onCancel)
            Asked(app, asked)
        }
    }
}

/**
 * Asking ErisAuth, as one block: on the front door, and in settings to sign in again — to ask
 * for what the person did not give the first time, or to reach a new ErisDB.
 */
@Composable
fun SignInPanel(app: String, state: SignIn, signedIn: Boolean, onAsk: () -> Unit, onCancel: () -> Unit) {
    val ask = if (signedIn) "Sign in again" else "Sign in with ErisAuth"
    when (state) {
        is SignIn.Asking -> {
            Row(verticalAlignment = Alignment.CenterVertically) {
                CircularProgressIndicator(Modifier.size(22.dp), strokeWidth = 2.dp)
                Text(
                    "Waiting for ErisAuth…",
                    style = MaterialTheme.typography.titleMedium,
                    modifier = Modifier.padding(start = 14.dp),
                )
            }
            Text(
                "Approve $app in ErisAuth: open its notification, choose what $app may do, and type " +
                    "ErisDB's passphrase.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            TextButton(onClick = onCancel) { Text("Cancel") }
        }

        is SignIn.NoErisAuth -> {
            Card(colors = CardDefaults.cardColors(MaterialTheme.colorScheme.errorContainer)) {
                Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text("ErisAuth is not on this phone", style = MaterialTheme.typography.titleSmall)
                    Text(
                        "Install ErisAuth and add this phone to your devices with it. It is how $app " +
                            "gets into your ErisDB, and there is no other way in.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }
            Button(onClick = onAsk) { Text(ask) }
        }

        is SignIn.Denied -> {
            Problem("ErisAuth did not let $app in: ${state.why}")
            Button(onClick = onAsk) { Text("Ask again") }
        }

        is SignIn.Failed -> {
            Problem("$app could not sign in: ${state.why}")
            Button(onClick = onAsk) { Text("Ask again") }
        }

        is SignIn.Ready, is SignIn.SignedIn -> Button(onClick = onAsk) { Text(ask) }
    }
}

@Composable
private fun Problem(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.error)
}

/** What the app asks for, as the person will see it in ErisAuth. */
@Composable
private fun Asked(app: String, asked: List<String>) {
    Card(colors = CardDefaults.cardColors(MaterialTheme.colorScheme.surfaceVariant)) {
        Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text("$app asks to", style = MaterialTheme.typography.labelLarge)
            asked.forEach { Text("· $it", style = MaterialTheme.typography.bodyMedium) }
            Text(
                "You can approve fewer of these. $app hides what it is not given.",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}
