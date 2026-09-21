package dev.erisdb.android

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.view.WindowManager
import androidx.compose.animation.AnimatedVisibility
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
import androidx.compose.material.icons.filled.ContentPaste
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.QrCodeScanner
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material.icons.filled.VisibilityOff
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp

/**
 * The front door: how this phone learns which erisdb it belongs to.
 *
 * Three ways in, in the order they are worth using. Scanning is first
 * because the phone already owns a camera app that reads QR codes and
 * opens the URI it finds — so `erisdb pair` shows a code and the scan lands
 * straight in here, with no scanner embedded, no camera permission asked
 * for, and no library to trust. Pasting is for a ticket that arrived as
 * text. Typing is for when neither is possible, and looks it.
 *
 * A scan or a paste starts a conversation and leads to WaitingScreen; the
 * typed route takes a capability token that already exists and skips it.
 */
@Composable
fun PairingScreen(
    coreName: String,
    server: String,
    token: String,
    status: String,
    connecting: Boolean,
    onTicket: (String) -> Unit,
    onManual: (String, String) -> Unit,
    onBack: (() -> Unit)?,
) {
    SecureWindow()
    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (onBack != null) {
                IconButton(onClick = onBack) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "back")
                }
            }
            Text(
                "Pair with an ErisDB",
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.padding(horizontal = if (onBack == null) 16.dp else 0.dp),
            )
        }
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                "Run erisdb pair on the core. Its code says where the core is and " +
                    "buys one conversation with it — this app then asks for the " +
                    "permissions it needs, and you approve them on the core.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            PairingPanel(
                coreName = coreName,
                server = server,
                token = token,
                status = status,
                connecting = connecting,
                onTicket = onTicket,
                onManual = onManual,
            )
        }
    }
}

/**
 * The three ways to pair, as one block — the same block on the front door
 * and in settings, because re-pairing is pairing.
 */
@Composable
fun PairingPanel(
    coreName: String,
    server: String,
    token: String,
    status: String,
    connecting: Boolean,
    onTicket: (String) -> Unit,
    onManual: (String, String) -> Unit,
) {
    var codeField by remember { mutableStateOf("") }
    var manual by remember { mutableStateOf(false) }
    var serverField by remember(server) { mutableStateOf(server) }
    var tokenField by remember(token) { mutableStateOf(token) }
    var revealed by remember { mutableStateOf(false) }

    if (paired(server, token)) {
        Text(
            "Paired with ${coreName.ifBlank { server.take(12) + "…" }}",
            style = MaterialTheme.typography.bodyLarge,
        )
    }

    // 1 — scan.
    Card(colors = CardDefaults.cardColors(MaterialTheme.colorScheme.surfaceVariant)) {
        Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Filled.QrCodeScanner, contentDescription = null)
                Text(
                    "Scan the QR with your camera app",
                    style = MaterialTheme.typography.titleSmall,
                    modifier = Modifier.padding(start = 10.dp),
                )
            }
            Text(
                "Point your phone's camera at the code erisdb pair prints and tap the " +
                    "link it offers. It opens this app already holding the ticket — " +
                    "no scanner in here to grant the camera to.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }

    // 2 — paste.
    Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(top = 4.dp)) {
        Icon(Icons.Filled.ContentPaste, contentDescription = null)
        Text(
            "Paste a pairing code",
            style = MaterialTheme.typography.titleSmall,
            modifier = Modifier.padding(start = 10.dp),
        )
    }
    OutlinedTextField(
        codeField,
        { codeField = it },
        Modifier.fillMaxWidth(),
        label = { Text("erisdb://pair/…") },
        maxLines = 3,
    )
    Button(
        onClick = { onTicket(codeField.trim()) },
        enabled = !connecting && codeField.isNotBlank(),
    ) { Text(if (connecting) "dialing…" else "Pair") }

    Text(
        status,
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )

    // 3 — type it all out. Secondary on purpose: it is the escape hatch,
    // not the route.
    TextButton(onClick = { manual = !manual }, modifier = Modifier.padding(top = 8.dp)) {
        Icon(
            if (manual) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
            contentDescription = null,
        )
        Text("Enter details manually", modifier = Modifier.padding(start = 6.dp))
    }
    AnimatedVisibility(manual) {
        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
            OutlinedTextField(
                serverField,
                { serverField = it },
                Modifier.fillMaxWidth(),
                label = { Text("iroh endpoint id") },
                singleLine = true,
            )
            OutlinedTextField(
                tokenField,
                { tokenField = it },
                Modifier.fillMaxWidth(),
                label = { Text("capability token (erisdb1.…)") },
                singleLine = true,
                visualTransformation =
                    if (revealed) VisualTransformation.None else PasswordVisualTransformation(),
                trailingIcon = {
                    IconButton(onClick = { revealed = !revealed }) {
                        Icon(
                            if (revealed) Icons.Filled.VisibilityOff else Icons.Filled.Visibility,
                            contentDescription = if (revealed) "hide token" else "show token",
                        )
                    }
                },
            )
            Button(
                onClick = { onManual(serverField.trim(), tokenField.trim()) },
                enabled = !connecting && serverField.isNotBlank() && tokenField.isNotBlank(),
            ) { Text(if (connecting) "dialing…" else "Connect") }
        }
    }
}

/**
 * Keep this screen out of screenshots and out of the recents thumbnail
 * while it is up. A ticket is a bearer credential rendered large, and so
 * is a token in a text field.
 */
@Composable
fun SecureWindow() {
    val activity = LocalContext.current.findActivity()
    DisposableEffect(activity) {
        activity?.window?.setFlags(
            WindowManager.LayoutParams.FLAG_SECURE,
            WindowManager.LayoutParams.FLAG_SECURE,
        )
        onDispose { activity?.window?.clearFlags(WindowManager.LayoutParams.FLAG_SECURE) }
    }
}

/** The activity behind a composable's context, however it is wrapped. */
fun Context.findActivity(): Activity? {
    var ctx = this
    while (ctx is ContextWrapper) {
        if (ctx is Activity) return ctx
        ctx = ctx.baseContext
    }
    return null
}
