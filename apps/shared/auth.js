/* Shared enrollment and renewal for the two browser clients. */
"use strict";
const ErisDBAuth = (() => {
  const b64 = bytes => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

  function baseURL(value) {
    const url = new URL(value);
    const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
    if (!(url.protocol === "https:" || (url.protocol === "http:" && local)) ||
        url.username || url.password || url.search || url.hash) {
      throw new Error("Use HTTPS for the core, or HTTP on localhost.");
    }
    return url.href.replace(/\/$/, "");
  }

  async function identity() {
    if (!globalThis.crypto?.subtle) throw new Error("Open this app over HTTPS or on localhost to pair securely.");
    const secret = b64(crypto.getRandomValues(new Uint8Array(32)));
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(secret));
    return { secret, challenge: b64(new Uint8Array(digest)) };
  }

  function clientId(token) {
    try {
      const id = JSON.parse(atob(token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/"))).client;
      return typeof id === "string" && /^[0-9a-f-]{36}$/.test(id) ? id : null;
    } catch { return null; }
  }

  async function renew(config) {
    const id = clientId(config.token);
    const registered = id && config.refreshSecret;
    const path = registered ? `/v1/clients/${id}/refresh` : "/v1/capabilities/refresh";
    const response = await fetch(baseURL(config.url) + path, {
      method: "POST", redirect: "error", cache: "no-store",
      headers: {
        "Content-Type": "application/json",
        ...(registered ? { "X-ErisDB-Client-Proof": config.refreshSecret } : { "Authorization": "Bearer " + config.token }),
      },
      body: JSON.stringify({ ttl_secs: config.ttl }),
    });
    return { status: response.status, body: await response.json() };
  }

  return Object.freeze({ baseURL, identity, clientId, renew });
})();
