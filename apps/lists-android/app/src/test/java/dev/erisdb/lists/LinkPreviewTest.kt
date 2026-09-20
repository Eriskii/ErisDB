package dev.erisdb.lists

import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.InetAddress

/**
 * Where a link preview may and may not go. Every address here is a
 * literal, so `getByName` parses rather than resolves and the test needs
 * no network.
 */
class LinkPreviewTest {

    private fun public(literal: String) = isPublicAddress(InetAddress.getByName(literal))

    @Test
    fun ordinaryInternetAddressesAreFetchable() {
        assertTrue(public("8.8.8.8"))
        assertTrue(public("93.184.216.34"))
        assertTrue(public("2606:2800:220:1:248:1893:25c8:1946"))
    }

    @Test
    fun thePhoneItselfIsNot() {
        assertFalse(public("127.0.0.1"))
        assertFalse(public("127.1.2.3"))
        assertFalse(public("0.0.0.0"))
        assertFalse(public("::1"))
    }

    @Test
    fun theNetworkThePhoneIsSittingOnIsNot() {
        assertFalse(public("10.0.0.1"))
        assertFalse(public("172.16.0.1"))
        assertFalse(public("172.31.255.254"))
        assertFalse(public("192.168.1.1"))
        assertFalse(public("169.254.169.254"))   // the cloud metadata address
        assertFalse(public("100.64.0.1"))        // carrier-grade NAT
        assertFalse(public("fc00::1"))           // unique local
        assertFalse(public("fe80::1"))           // link-local
    }

    @Test
    fun reservedAndBroadcastRangesAreNot() {
        assertFalse(public("224.0.0.1"))
        assertFalse(public("240.0.0.1"))
        assertFalse(public("255.255.255.255"))
    }

    @Test
    fun anIpv4AddressWearingAnIpv6CoatIsStillLoopback() {
        assertFalse(public("::ffff:127.0.0.1"))
    }

    @Test
    fun aBareHostnameIsReadAsHttps() {
        assertEqualsUrl("https://example.com", previewUrl("example.com"))
        assertEqualsUrl("http://example.com/x", previewUrl("http://example.com/x"))
    }

    @Test
    fun onlyTheWebSchemesAreEverFetched() {
        assertNull(previewUrl("file:///etc/passwd")?.takeIf { fetchable(it) })
        // `file:` has no host, so it never even reaches a resolver.
        assertFalse(fetchable(java.net.URL("file:///etc/passwd")))
    }

    private fun assertEqualsUrl(expected: String, actual: java.net.URL?) {
        org.junit.Assert.assertEquals(expected, actual?.toString())
    }
}
