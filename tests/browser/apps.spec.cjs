// Tasks and Lists in a real browser, signing in through a real ErisAuth and working against a
// real ErisDB, every participant reaching the others through one Iroh relay. The tests act as
// the person at the device (ErisAuth's socket, as its approval page does) and, where they look
// at what ErisDB holds, as its operator over HTTP. Nothing is intercepted or simulated.
"use strict";
const { test, expect } = require("@playwright/test");

let world;

test.beforeAll(async ({ request }) => {
  world = await (await request.get("/harness")).json();
});

const APPS = {
  tasks: { name: "Tasks (Web)", client: "Tasks (Web) v0.3", list: "#list", label: body => body.title },
  lists: { name: "Lists (Web)", client: "Lists (Web) v0.3", list: "#entries", label: body => body.name },
};

const wanted = app => ["read", "create", "update", "delete"].map(action => `${app}:${action}`);

/** One command to ErisAuth on the person's device. */
async function erisauth(request, line) {
  const response = await request.post("/harness/erisauth", { data: line });
  expect(response.ok()).toBe(true);
  return response.json();
}

/** The request waiting for the person, once there is one. */
async function waiting(request) {
  let prompt;
  await expect.poll(async () => {
    prompt = (await erisauth(request, { op: "state" })).approvals[0];
    return prompt ?? null;
  }, { timeout: 30_000 }).not.toBeNull();
  return prompt;
}

/** ErisDB's API as its operator, over HTTP with a minted token. */
async function operator(request, method, route, data) {
  return request.fetch(world.core + route, { method, data, headers: { Authorization: "Bearer " + world.token } });
}

async function items(request, facet) {
  const response = await operator(request, "GET", `/v1/items?facet=${facet}&limit=1000`);
  expect(response.status()).toBe(200);
  return (await response.json()).items;
}

/** The schema an app registered for itself, if any. */
async function registration(request, app) {
  return (await items(request, "facet")).find(item => item.body.name === app);
}

async function stored(request, app, label) {
  return (await items(request, app)).find(item => APPS[app].label(item.body) === label);
}

/** Open a ticket on the device, as "Add a browser app" does, and hand it to the page. */
async function askByTicket(page, request) {
  const opened = await erisauth(request, { op: "add_app" });
  expect(opened.ticket).toMatch(/^eris:\/\/pair\//);
  await page.locator("#cfg-ticket").fill(opened.ticket);
  await page.locator("#cfg-pair").click();
  await expect(page.locator("#pair-fingerprint")).toHaveText(/^[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}$/, { timeout: 30_000 });
  return waiting(request);
}

const appId = page => page.evaluate(async () => (await erisdbWeb()).appId(appKey()));

async function add(page, app, label) {
  if (app === "tasks") await page.locator("#title").fill(label);
  else {
    await page.locator("#f-list").fill("Browser E2E");
    await page.locator("#f-name").fill(label);
  }
  await page.locator("#addform button").click();
}

const outbox = (page, app) => page.evaluate(app => JSON.parse(localStorage.getItem(`erisdb.${app}.outbox`) ?? "[]").length, app);

for (const app of ["tasks", "lists"]) {
  test(`${app}: signs in by ticket, works through ErisDB, hears changes, and is cut off when its note is revoked`, async ({ page, request }) => {
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    const { name, client, list } = APPS[app];

    await page.goto(`/${app}/`);
    await expect(page.locator("body")).toHaveClass(/signed-out/);
    // The app works against an ErisDB that has never seen its schema.
    const facet = await page.evaluate(() => FACET_BODY);
    expect(await registration(request, app)).toBeUndefined();

    // Both screens show the same fingerprint, and the device knows the app by its key.
    const prompt = await askByTicket(page, request);
    const id = await appId(page);
    expect(prompt).toMatchObject({ kind: "app", via: "ticket", name, key: id, service: "erisdb", grants: wanted(app), serves: false, passphrase: true });
    await expect(page.locator("#pair-fingerprint")).toHaveText(prompt.fingerprint);
    await expect(page.locator("#pair-wait-who")).toHaveText("laptop");

    // The page goes away while the person decides; the next one waits for the same answer.
    await page.reload();
    await expect(page.locator("#pair-fingerprint")).toHaveText(prompt.fingerprint);
    await expect(page.locator("#pair-wait")).toBeVisible();

    // A wrong passphrase lets the person try again, and the app hears nothing until ErisDB accepts.
    const wrong = await erisauth(request, { op: "decide", id: prompt.id, passphrase: "a wrong guess" });
    expect(wrong.error).toBe("wrong_passphrase");
    expect((await erisauth(request, { op: "state" })).approvals.map(a => a.id)).toEqual([prompt.id]);
    await expect(page.locator("#pair-wait")).toBeVisible();
    expect(await erisauth(request, { op: "decide", id: prompt.id, passphrase: world.passphrase })).toEqual({ ok: true });
    await expect(page.locator("#pair-wait")).toBeHidden({ timeout: 30_000 });
    await expect(page.locator("body")).not.toHaveClass(/signed-out/);
    await expect(page.locator("#cfg-state")).toContainText("laptop");

    // Signed in, the app registers its schema itself, as its own key.
    await expect.poll(async () => (await registration(request, app))?.source.addr, { timeout: 30_000 }).toBe("iroh:" + id);
    expect((await registration(request, app)).body).toEqual(facet);

    // Create, update and delete through the app; ErisDB holds the result, each write the app's.
    const label = `${app}-${Date.now()}`;
    await add(page, app, label);
    await expect.poll(async () => (await stored(request, app, label))?.source, { timeout: 30_000 })
      .toMatchObject({ addr: "iroh:" + id, client });
    const item = await stored(request, app, label);
    if (app === "tasks") await page.locator("#list li").filter({ hasText: label }).getByRole("checkbox").check();
    else {
      await page.locator("#entries .row").filter({ hasText: label }).click();
      await page.getByPlaceholder("description", { exact: true }).fill("Edited through Lists");
      await page.getByRole("button", { name: "Save", exact: true }).click();
    }
    await expect.poll(async () => {
      const changed = await stored(request, app, label);
      return changed.revision > item.revision && (app === "tasks" ? changed.body.done : changed.body.description === "Edited through Lists");
    }, { timeout: 30_000 }).toBe(true);
    if (app === "tasks") await page.locator("#list li").filter({ hasText: label }).getByTitle("delete", { exact: true }).click();
    else {
      await page.locator("#entries .row").filter({ hasText: label }).click();
      await page.getByRole("button", { name: "Delete", exact: true }).click();
    }
    await expect.poll(async () => (await operator(request, "GET", `/v1/items/${item.id}`)).status(), { timeout: 30_000 }).toBe(404);

    // Another client writes; the change arrives on the open feed, with no reload.
    await expect(page.locator("#status")).toContainText("live", { timeout: 30_000 });
    const elsewhere = `${app}-elsewhere-${Date.now()}`;
    const written = await operator(request, "POST", "/v1/items", { facet: app, body: app === "tasks" ? { title: elsewhere, done: false } : { list: "Elsewhere", name: elsewhere } });
    expect(written.status()).toBe(201);
    await expect(page.locator(list)).toContainText(elsewhere, { timeout: 5_000 });

    // The person revokes the app's note on the device: ErisDB cuts it off at once, and the
    // page says so.
    const note = (await erisauth(request, { op: "state" })).notes.find(note => note.app === id);
    expect(await erisauth(request, { op: "revoke", note: note.id })).toEqual({ ok: true });
    await expect(page.locator("#status")).toContainText("no longer lets this app in", { timeout: 30_000 });
    await expect(page.locator("#pair-msg")).toContainText("revoked");
    // Its next request fails the same way, and what it tried to write waits on this device.
    const refused = `${app}-refused-${Date.now()}`;
    await add(page, app, refused);
    await expect(page.locator(list)).toContainText(refused);
    await expect(page.locator("#status")).toContainText("no longer lets this app in");
    expect(await outbox(page, app)).toBe(1);
    expect(await stored(request, app, refused)).toBeUndefined();

    // Signed in again by a new ticket, under the same key, the app is let back in and the
    // write that waited lands.
    const again = await askByTicket(page, request);
    expect(again.key).toBe(id);
    expect(await erisauth(request, { op: "decide", id: again.id, passphrase: world.passphrase })).toEqual({ ok: true });
    await expect.poll(async () => (await stored(request, app, refused))?.source.addr, { timeout: 30_000 }).toBe("iroh:" + id);
    await expect(page.locator("#status")).not.toContainText("no longer lets this app in");

    // Signing out forgets the key and everything the app held.
    await page.locator("#settings > summary").click();
    await page.locator("#cfg-forget").click();
    await expect(page.locator("body")).toHaveClass(/signed-out/);
    expect(await page.evaluate(() => Object.keys(localStorage).filter(key => key.startsWith("erisdb.")))).toEqual([]);
    expect(errors).toEqual([]);
  });
}

test("the person approves part of what an app asks, and the app offers only that", async ({ page, request }) => {
  await page.goto("/lists/");
  const prompt = await askByTicket(page, request);
  const decided = await erisauth(request, { op: "decide", id: prompt.id, grants: ["lists:read"], passphrase: world.passphrase });
  expect(decided).toEqual({ ok: true });
  await expect(page.locator("#perm-banner")).toContainText("Read-only", { timeout: 30_000 });
  await expect(page.locator("#add-card")).toBeHidden();
});

test("a ticket that is not one is refused, and the person saying no gives the app nothing", async ({ page, request }) => {
  await page.goto("/tasks/");
  await page.locator("#cfg-ticket").fill("erisdb://pair/eyJ2IjoxfQ");
  await page.locator("#cfg-pair").click();
  await expect(page.locator("#pair-msg")).toContainText("eris://pair/");
  await page.locator("#cfg-ticket").fill("eris://pair/not-a-ticket");
  await page.locator("#cfg-pair").click();
  await expect(page.locator("#pair-msg")).toContainText("damaged");

  const prompt = await askByTicket(page, request);
  const id = await appId(page);
  expect(await erisauth(request, { op: "deny", id: prompt.id })).toEqual({ ok: true });
  await expect(page.locator("#pair-msg")).toContainText("said no", { timeout: 30_000 });
  await expect(page.locator("#pair-wait")).toBeHidden();
  await expect(page.locator("body")).toHaveClass(/signed-out/);
  expect(await page.evaluate(() => localStorage.getItem("erisdb.tasks.config"))).toBeNull();
  expect((await erisauth(request, { op: "state" })).notes.some(note => note.app === id)).toBe(false);
});

test("writes made while ErisDB is away wait on the device, outlive a reload, and land once it is back", async ({ context, request }) => {
  const pages = {};
  for (const app of ["tasks", "lists"]) {
    const page = pages[app] = await context.newPage();
    await page.goto(`/${app}/`);
    const prompt = await askByTicket(page, request);
    expect(await erisauth(request, { op: "decide", id: prompt.id, passphrase: world.passphrase })).toEqual({ ok: true });
    await expect(page.locator("#status")).toContainText("live", { timeout: 30_000 });
  }
  // ErisDB goes away, and the apps' schemas with it.
  for (const app of ["tasks", "lists"]) {
    const schema = await registration(request, app);
    expect((await operator(request, "DELETE", `/v1/items/${schema.id}`)).status()).toBe(204);
  }
  expect((await request.post("/harness/erisdb/stop")).ok()).toBe(true);
  const labels = {};
  for (const [app, page] of Object.entries(pages)) {
    labels[app] = `${app}-offline-${Date.now()}`;
    await add(page, app, labels[app]);
    await expect.poll(() => outbox(page, app)).toBe(1);
    await expect(page.locator(APPS[app].list)).toContainText(labels[app]);
    await page.reload();
    await expect(page.locator(APPS[app].list)).toContainText(labels[app]);
    expect(await outbox(page, app)).toBe(1);
  }
  // Back, ErisDB takes each write, each app registering its schema again first.
  expect((await request.post("/harness/erisdb/start", { timeout: 120_000 })).ok()).toBe(true);
  for (const [app, page] of Object.entries(pages)) {
    const id = await appId(page);
    await expect.poll(async () => (await stored(request, app, labels[app]))?.source.addr, { timeout: 90_000 }).toBe("iroh:" + id);
    expect((await registration(request, app))?.source.addr).toBe("iroh:" + id);
    await expect.poll(() => outbox(page, app)).toBe(0);
  }
});
