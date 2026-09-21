const { test, expect } = require("@playwright/test");
const { execFileSync } = require("node:child_process");
const path = require("node:path");
const core = "http://127.0.0.1:18771";
let admin;

test.beforeAll(() => {
  admin = execFileSync(path.resolve("erisdb/target/debug/erisdb"), ["mint", "--grant", "*", "--ttl", "86400", "--secret", "erisdb-browser-e2e-only"], { encoding: "utf8" }).trim();
});

async function api(request, method, route, data, token = admin, extra = {}) {
  return request.fetch(core + route, { method, data, headers: { Authorization: "Bearer " + token, ...extra } });
}

async function ticket(request, name) {
  const response = await api(request, "POST", "/v1/pairings", {});
  expect(response.status()).toBe(201);
  const session = await response.json();
  return { ...session, ticket: "erisdb://pair/" + Buffer.from(JSON.stringify({ v: 1, name, url: core, token: session.secret })).toString("base64url") };
}

for (const app of ["tasks", "lists"]) {
  test(`${app}: fresh setup, offline restart, CRUD, renewal, and revocation`, async ({ page: firstPage, context, request }) => {
    let page = firstPage;
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.goto(`/apps/${app}/`);
    // The app must work on a core that has never seen its schema.
    const facet = await page.evaluate(() => FACET_BODY);
    const definitions = await (await api(request, "GET", "/v1/items?facet=facet")).json();
    expect(definitions.items.some(item => item.body.name === app)).toBe(false);

    const session = await ticket(request, "Browser E2E");
    await page.locator("#cfg-ticket").fill(session.ticket);
    await page.locator("#cfg-pair").click();
    await expect(page.locator("#pair-wait-what")).toContainText("Compare");
    const pending = await (await api(request, "GET", `/v1/pairings/${session.id}`)).json();
    await expect(page.locator("#pair-wait-what")).toContainText(pending.body.fingerprint);
    expect(pending.body.requested).toEqual([`${app}:read`, `${app}:create`, `${app}:update`, `${app}:delete`]);

    // Reload while the person is approving: the same proof must survive.
    await page.reload();
    await expect(page.locator("#pair-wait-what")).toContainText(pending.body.fingerprint);

    // A copied ticket cannot collect this browser's approval.
    const stolen = await api(request, "GET", "/v1/pair/status", undefined, session.secret);
    expect(stolen.status()).toBe(401);
    const approved = await api(request, "POST", `/v1/pairings/${session.id}/approve`, { granted: [`${app}:read`, `${app}:create`], ttl_secs: 3 });
    expect(approved.status()).toBe(200);
    await expect(page.locator("#pair-wait")).toBeHidden();
    // Lose the schema while disconnected. The app must keep the user's write
    // and initialize again after restart, without administrator registration.
    await expect.poll(async () => {
      const data = await (await api(request, "GET", "/v1/items?facet=facet")).json();
      return data.items.some(item => item.body.name === app);
    }).toBe(true);
    await context.setOffline(true);
    const initialized = await (await api(request, "GET", "/v1/items?facet=facet")).json();
    const schemaId = initialized.items.find(item => item.body.name === app).id;
    expect((await api(request, "DELETE", `/v1/items/${schemaId}`)).status()).toBe(204);
    const label = `${app}-real-${Date.now()}`;
    if (app === "tasks") await page.locator("#title").fill(label);
    else {
      await page.locator("#f-list").fill("Real E2E");
      await page.locator("#f-name").fill(label);
    }
    await page.locator("#addform button").click();
    await expect.poll(() => page.evaluate(app => JSON.parse(localStorage.getItem(`erisdb.${app}.outbox`)).length, app)).toBe(1);
    expect((await (await api(request, "GET", `/v1/items?facet=${app}`)).json()).items).toEqual([]);
    await page.close();
    await context.setOffline(false);
    page = await context.newPage();
    page.on("pageerror", error => errors.push(error.message));
    await page.goto(`/apps/${app}/`);
    await expect.poll(async () => {
      const data = await (await api(request, "GET", `/v1/items?facet=${app}`)).json();
      return data.items.some(item => (item.body.title ?? item.body.name) === label && item.source.installation === session.id);
    }).toBe(true);
    const after = await (await api(request, "GET", "/v1/items?facet=facet")).json();
    const registered = after.items.find(item => item.body.name === app);
    expect(registered.body).toEqual(facet);
    expect(registered.source.installation).toBe(session.id);
    // The real schema is active, rather than a permissive fixture definition.
    expect((await api(request, "POST", "/v1/items", { facet: app, body: {} })).status()).toBe(422);
    const saved = await page.evaluate(app => JSON.parse(localStorage.getItem(`erisdb.${app}.config`)), app);
    expect(saved.refreshSecret).toHaveLength(43);
    // Wait for actual expiry; no simulated clock or intercepted responses.
    await expect.poll(() => Date.now() / 1000, { timeout: 6000 }).toBeGreaterThan(JSON.parse(Buffer.from(saved.token.split(".")[1], "base64url")).exp);
    // An actual authorization failure can renew too, independent of the UI clock.
    const rejection = page.waitForResponse(response => response.url().endsWith("/v1/permissions") &&
      response.status() === 401 && response.request().headers().authorization === "Bearer " + saved.token);
    const recovered = await page.evaluate(expiredToken => {
      // Restore a genuinely issued, now expired credential, as an old backup would.
      config = { ...config, token: expiredToken };
      return api("GET", "/v1/permissions");
    }, saved.token);
    await rejection;
    expect(recovered.status).toBe(200);
    const recoveredToken = await page.evaluate(app => JSON.parse(localStorage.getItem(`erisdb.${app}.config`)).token, app);
    await expect.poll(() => Date.now() / 1000, { timeout: 6000 }).toBeGreaterThan(JSON.parse(Buffer.from(recoveredToken.split(".")[1], "base64url")).exp);
    const renewal = page.waitForResponse(response => response.url().endsWith(`/v1/clients/${session.id}/refresh`) && response.status() === 200);
    await page.reload();
    await renewal;
    await expect(page.locator(app === "tasks" ? "#list" : "#entries")).toContainText(label);
    const resumed = await page.evaluate(app => JSON.parse(localStorage.getItem(`erisdb.${app}.config`)), app);
    expect(resumed.token).not.toBe(saved.token);

    // Pair this same browser installation again; its identity stays singular.
    const again = await ticket(request, "Same core");
    await page.locator("#settings > summary").click();
    await page.locator("#cfg-ticket").fill(again.ticket);
    await page.locator("#cfg-pair").click();
    await expect(page.locator("#pair-wait-what")).toContainText("Compare");
    expect((await api(request, "POST", `/v1/pairings/${again.id}/approve`, { granted: [`${app}:read`, `${app}:create`, `${app}:update`, `${app}:delete`], ttl_secs: 3 })).status()).toBe(200);
    await expect(page.locator("#pair-wait")).toBeHidden();
    const pairedAgain = await page.evaluate(app => JSON.parse(localStorage.getItem(`erisdb.${app}.config`)), app);
    expect(pairedAgain.refreshSecret).toBe(saved.refreshSecret);
    expect(JSON.parse(Buffer.from(pairedAgain.token.split(".")[1], "base64url")).client).toBe(session.id);

    // Edit and delete through the app, then confirm authoritative server state.
    const listing = await (await api(request, "GET", `/v1/items?facet=${app}`)).json();
    const item = listing.items.find(item => (item.body.title ?? item.body.name) === label);
    if (app === "tasks") await page.locator("#list li").filter({ hasText: label }).getByRole("checkbox").check();
    else {
      await page.locator("#entries .row").filter({ hasText: label }).click();
      await page.getByPlaceholder("description", { exact: true }).fill("Edited through Lists");
      await page.getByRole("button", { name: "Save", exact: true }).click();
    }
    await expect.poll(async () => {
      const changed = await (await api(request, "GET", `/v1/items/${item.id}`)).json();
      return changed.revision > item.revision && changed.source.installation === session.id &&
        (app === "tasks" ? changed.body.done : changed.body.description === "Edited through Lists");
    }).toBe(true);
    if (app === "tasks") await page.locator("#list li").filter({ hasText: label }).getByTitle("delete", { exact: true }).click();
    else {
      await page.locator("#entries .row").filter({ hasText: label }).click();
      await page.getByRole("button", { name: "Delete", exact: true }).click();
    }
    await expect.poll(async () => (await api(request, "GET", `/v1/items/${item.id}`)).status()).toBe(404);

    const record = await (await api(request, "GET", `/v1/clients/${session.id}`)).json();
    expect((await api(request, "PUT", `/v1/clients/${session.id}`, { grants: [`${app}:read`], revision: record.revision })).status()).toBe(200);
    await page.reload();
    await expect(page.locator("#add-card")).toBeHidden();
    expect((await api(request, "POST", `/v1/clients/${session.id}/revoke`, {})).status()).toBe(200);
    await page.reload();
    await expect(page.locator("#status")).toContainText("revoked");
    const denied = await api(request, "POST", `/v1/clients/${session.id}/refresh`, {}, saved.token, { "X-ErisDB-Client-Proof": saved.refreshSecret });
    expect(denied.status()).toBe(401);
    expect(errors).toEqual([]);
  });
}

test("denial grants nothing and unsafe remote HTTP tickets are refused", async ({ page, request }) => {
  await page.goto("/apps/tasks/");
  const session = await ticket(request, "Denied");
  await page.locator("#cfg-ticket").fill(session.ticket);
  await page.locator("#cfg-pair").click();
  await expect(page.locator("#pair-wait-what")).toContainText("Compare");
  expect((await api(request, "POST", `/v1/pairings/${session.id}/deny`, {})).status()).toBe(200);
  await expect(page.locator("#pair-msg")).toContainText("refused");
  expect(await page.evaluate(() => localStorage.getItem("erisdb.tasks.config"))).toBeNull();
  const unsafe = "erisdb://pair/" + Buffer.from(JSON.stringify({ v: 1, url: "http://192.168.1.25:7700", token: session.secret })).toString("base64url");
  await page.locator("#cfg-ticket").fill(unsafe);
  await page.locator("#cfg-pair").click();
  await expect(page.locator("#pair-msg")).toContainText("HTTPS");
});
