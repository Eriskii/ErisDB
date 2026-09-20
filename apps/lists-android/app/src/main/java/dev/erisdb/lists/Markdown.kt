package dev.erisdb.lists

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import coil.compose.AsyncImage
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject

// ---------------------------------------------------------------- markdown
// A deliberately small inline renderer: bold, italic, code, links, bullet
// and heading markers. Enough for card descriptions; not a spec engine.

private val INLINE = Regex("""\*\*(.+?)\*\*|\*(.+?)\*|`(.+?)`|\[(.+?)]\((\S+?)\)""")

fun markdownToAnnotated(src: String): AnnotatedString = buildAnnotatedString {
    src.lines().forEachIndexed { i, raw ->
        if (i > 0) append('\n')
        var line = raw.trim()
        line = line.removePrefix("### ").removePrefix("## ").removePrefix("# ")
        if (line.startsWith("- ") || line.startsWith("* ")) line = "• " + line.drop(2)
        var cursor = 0
        for (m in INLINE.findAll(line)) {
            append(line.substring(cursor, m.range.first))
            when {
                m.groups[1] != null -> withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { append(m.groups[1]!!.value) }
                m.groups[2] != null -> withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { append(m.groups[2]!!.value) }
                m.groups[3] != null -> withStyle(SpanStyle(fontFamily = FontFamily.Monospace)) { append(m.groups[3]!!.value) }
                else -> withStyle(SpanStyle(textDecoration = TextDecoration.Underline)) { append(m.groups[4]!!.value) }
            }
            cursor = m.range.last + 1
        }
        append(line.substring(cursor))
    }
}

// ---------------------------------------------------------------- images
// An item's picture: the explicit `image` attribute, which the entry
// carries and costs nothing to show. Failing that, the link's og:image —
// but only where the person holding the phone has asked for link
// previews, because resolving one means connecting to the linked site.
// See LinkPreview.kt.

/** The URL an item's card image loads from, or null. */
fun explicitImage(body: JSONObject): String? =
    body.optJSONObject("attributes")?.optString("image")?.ifBlank { null }

@Composable
fun ItemImage(body: JSONObject, modifier: Modifier = Modifier) {
    val previews = LocalLinkPreviews.current
    val explicit = explicitImage(body)
    val link = body.optString("link").ifBlank { null }
    var url by remember(explicit, link, previews) {
        mutableStateOf(explicit ?: link?.let { cachedSiteImage(it) })
    }
    LaunchedEffect(explicit, link, previews) {
        if (previews && explicit == null && link != null && url == null) {
            url = withContext(Dispatchers.IO) { resolveSiteImage(link) }
        }
    }
    val resolved = url
    if (!resolved.isNullOrEmpty()) {
        AsyncImage(
            model = resolved,
            contentDescription = null,
            contentScale = ContentScale.Crop,
            modifier = modifier
                .fillMaxWidth()
                .padding(top = 8.dp)
                .height(160.dp)
                .clip(RoundedCornerShape(8.dp)),
        )
    }
}
