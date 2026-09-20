package dev.erisdb.lists

import androidx.compose.runtime.staticCompositionLocalOf
import java.net.HttpURLConnection
import java.net.InetAddress
import java.net.URL

// An entry's card can show the picture the linked page advertises —
// og:image or twitter:image. Doing that means this phone connecting to
// whatever address the entry names, which is a decision for the person
// holding the phone and not for the renderer: it tells the site someone
// is looking, from this IP, and the site chooses what comes back.
//
// So it is off until switched on, and it will only talk to the public
// internet. A link pointing at localhost or at the network the phone is
// sitting on is not fetched, at any hop of any redirect chain — an entry
// is data, and data must not turn the app into a probe of the LAN it
// happens to be on.

/** Whether an entry's link may be fetched for a preview image. */
val LocalLinkPreviews = staticCompositionLocalOf { false }

/** Redirects are followed by hand: HttpURLConnection will not cross
 * http→https, and every hop needs checking anyway. */
private const val MAX_HOPS = 4

/** Resolved previews are remembered, but not without limit — this used to
 * be a map that only ever grew. */
private const val CACHE_LIMIT = 128

/** `""` means "looked, found nothing", which is worth remembering too. */
private val cache = object : LinkedHashMap<String, String>(16, 0.75f, true) {
    override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, String>?): Boolean =
        size > CACHE_LIMIT
}

private val OG_PATTERNS = listOf(
    Regex("""(?:property|name)=["'](?:og:image|twitter:image)["'][^>]*?content=["']([^"']+)""", RegexOption.IGNORE_CASE),
    Regex("""content=["']([^"']+)["'][^>]*?(?:property|name)=["'](?:og:image|twitter:image)["']""", RegexOption.IGNORE_CASE),
)

/**
 * True when an address is out on the public internet — not this phone,
 * not the network it is sitting on, not a range that means something
 * special to somebody.
 */
fun isPublicAddress(addr: InetAddress): Boolean {
    if (addr.isAnyLocalAddress || addr.isLoopbackAddress || addr.isLinkLocalAddress ||
        addr.isSiteLocalAddress || addr.isMulticastAddress
    ) return false
    val bytes = addr.address
    return when (bytes.size) {
        4 -> {
            val a = bytes[0].toInt() and 0xff
            val b = bytes[1].toInt() and 0xff
            when {
                a == 0 -> false                     // 0.0.0.0/8, "this network"
                a == 127 -> false                   // loopback
                a == 100 && b in 64..127 -> false   // 100.64/10, carrier-grade NAT
                a == 169 && b == 254 -> false       // link-local
                a >= 240 -> false                   // reserved, and broadcast
                else -> true
            }
        }
        // fc00::/7, unique local — the IPv6 equivalent of a private range.
        16 -> (bytes[0].toInt() and 0xfe) != 0xfc
        else -> false
    }
}

/**
 * True when this URL may be fetched: an ordinary web scheme, and a host
 * that resolves only to public addresses. A name answering with both a
 * public and a private address is refused outright — that shape is a
 * rebinding attempt and never an accident.
 */
fun fetchable(url: URL): Boolean {
    if (url.protocol != "http" && url.protocol != "https") return false
    val addresses = runCatching { InetAddress.getAllByName(url.host) }.getOrNull() ?: return false
    return addresses.isNotEmpty() && addresses.all { isPublicAddress(it) }
}

/** Users paste "example.com"; URL() needs a scheme. */
fun previewUrl(link: String): URL? = runCatching {
    URL(if (link.startsWith("http://") || link.startsWith("https://")) link else "https://$link")
}.getOrNull()

/**
 * The image the linked page advertises, or `""` when there is none and
 * when there is no fetching it. Blocking; call from an IO dispatcher.
 */
fun resolveSiteImage(link: String): String {
    synchronized(cache) { cache[link] }?.let { return it }
    val found = runCatching { fetchPreview(link) }.getOrNull() ?: ""
    synchronized(cache) { cache[link] = found }
    return found
}

/** What has already been resolved for this link, without going and asking. */
fun cachedSiteImage(link: String): String? = synchronized(cache) { cache[link] }

private fun fetchPreview(link: String): String? {
    var url = previewUrl(link) ?: return null
    var head = ""
    for (hop in 0 until MAX_HOPS) {
        if (!fetchable(url)) return null
        val conn = (url.openConnection() as HttpURLConnection).apply {
            connectTimeout = 5000
            readTimeout = 5000
            instanceFollowRedirects = false
            // Honest about who is asking. A site that would rather not
            // answer this is entitled to say so.
            setRequestProperty("User-Agent", "Mozilla/5.0 (compatible; erisdb-lists link preview)")
        }
        val code = conn.responseCode
        if (code in 300..399) {
            val location = conn.getHeaderField("Location") ?: return null
            conn.disconnect()
            url = URL(url, location)
            continue
        }
        // Read up to 128k chars or </head>: one read() only returns the
        // first chunk, which routinely misses the meta tags.
        head = conn.inputStream.bufferedReader().use { r ->
            val sb = StringBuilder()
            val buf = CharArray(8192)
            while (sb.length < 131_072) {
                val n = r.read(buf)
                if (n <= 0) break
                sb.append(buf, 0, n)
                if (sb.indexOf("</head>") >= 0) break
            }
            sb.toString()
        }
        break
    }
    val found = OG_PATTERNS.firstNotNullOfOrNull { it.find(head)?.groupValues?.get(1) } ?: return null
    val image = runCatching { URL(url, found) }.getOrNull() ?: return null
    return if (fetchable(image)) image.toString() else null
}
