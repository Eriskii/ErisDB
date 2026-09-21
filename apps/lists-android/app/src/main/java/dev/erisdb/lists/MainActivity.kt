package dev.erisdb.lists

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import dev.erisdb.lists.theme.ErisDBListsTheme

class MainActivity : ComponentActivity() {

  /**
   * The `erisdb://pair/…` this activity was opened with, if any.
   *
   * The phone's own camera app reads the QR code `erisdb pair` prints and
   * opens the URI in it, which the manifest's intent filter routes here.
   * That is the whole scanner: no camera permission, no library, and the
   * app that already has the camera does the part it is good at.
   */
  private var pairUri by mutableStateOf<String?>(null)

  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    enableEdgeToEdge()
    pairUri = pairingUri(intent)
    setContent {
      ErisDBListsTheme {
        // The surface fills the whole window — its color shows through the
        // transparent status/nav bars — while the content stays inset.
        Surface(
          modifier = Modifier.fillMaxSize(),
          color = MaterialTheme.colorScheme.background,
        ) {
          Box(Modifier.safeDrawingPadding()) {
            ListsApp(pairUri = pairUri, onPairHandled = { pairUri = null })
          }
        }
      }
    }
  }

  /** A scan while the app is already up. singleTop keeps this the same
   * activity, so the ticket arrives here rather than on a second copy. */
  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    setIntent(intent)
    pairUri = pairingUri(intent)
  }

  /** The ticket in an intent, or null when it carries none. Reading the
   * scheme here keeps a launcher tap from being mistaken for a pairing. */
  private fun pairingUri(intent: Intent?): String? {
    val data = intent?.data ?: return null
    return if (data.scheme == "erisdb" && data.host == "pair") data.toString() else null
  }
}
