// The browser suite's world, as real processes: an Iroh relay in plain-HTTP mode, a device
// running ErisOverlayNetwork that starts a mesh, ErisDB's core on PostgreSQL in Docker, approved
// through that device as the mesh's database (`database/1`) with its passphrase set, and the
// static apps. Every participant reaches the others through the relay alone (ERIS_NETWORK), as a
// browser must.
//
// Beside the apps, under /harness/, the tests act as the person at the device and as ErisDB's
// operator: they speak to ErisOverlayNetwork's socket, stop and start the core, and read where things are.
"use strict";
const { execFileSync, spawn } = require("node:child_process");
const { createServer } = require("node:http");
const { readFile, mkdtemp, rm } = require("node:fs/promises");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");

const root = path.resolve(__dirname, "../..");
const apps = path.join(root, "apps");
const PORT = 18770;
const CORE = "127.0.0.1:18771";
const SECRET = "erisdb-browser-e2e-only";
const PASSPHRASE = "correct horse battery";
/** What ErisDB serves, and what Tasks and Lists use. */
const DATABASE = "database/1";
const eonSource = path.resolve(root, process.env.EON_SOURCE || "../ErisOverlayNetwork");
const target = path.join(__dirname, "target");

const children = new Set();
let postgres, scratch, http, core;
let stopping = false;

function log(...what) { console.error("[harness]", ...what); }

function stop() {
  if (stopping) return;
  stopping = true;
  http?.close();
  for (const child of children) child.kill("SIGTERM");
  if (postgres) { try { execFileSync("docker", ["rm", "-f", postgres], { stdio: "ignore" }); } catch {} }
  if (scratch) fs.rmSync(scratch, { recursive: true, force: true });
}
process.once("SIGTERM", () => { stop(); process.exit(0); });
process.once("SIGINT", () => { stop(); process.exit(0); });
process.once("exit", stop);

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

async function until(what, check, ms = 120_000) {
  const deadline = Date.now() + ms;
  for (;;) {
    let last;
    try { last = await check(); if (last) return last; } catch (error) { last = error; }
    if (Date.now() > deadline) throw new Error(`never: ${what} (${last?.message ?? JSON.stringify(last)})`);
    await sleep(100);
  }
}

// From the filesystem root, as scripts/run-local.py builds: Cargo reads configuration from every
// directory above the one it runs in, and a workspace around this checkout that patches its
// dependencies would otherwise rewrite the lockfiles.
function cargo(args, env = {}) {
  execFileSync("cargo", args, { cwd: path.parse(root).root, stdio: "inherit", env: { ...process.env, ...env } });
}

/** A child that lives as long as the harness, its output on ours. */
function run(binary, args, env, options = {}) {
  const child = spawn(binary, args, { env: { ...process.env, ...env }, stdio: options.stdio ?? ["ignore", "inherit", "inherit"] });
  children.add(child);
  child.once("exit", () => children.delete(child));
  return child;
}

/** Run to completion, `input` on stdin; throws with its output when it fails. */
function finish(child, input) {
  return new Promise((resolve, reject) => {
    let output = "";
    child.stdout?.on("data", chunk => { output += chunk; });
    child.stderr?.on("data", chunk => { output += chunk; });
    if (input !== undefined) child.stdin.end(input);
    child.once("exit", code => code === 0 ? resolve(output) : reject(new Error(`exited ${code}: ${output}`)));
  });
}

// ---------------------------------------------------------------- ErisOverlayNetwork
// The person's device. Its socket takes the same commands its approval page sends.

let eon;

function say(line) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(eon.socket);
    let answer = "";
    socket.once("connect", () => socket.write(JSON.stringify(line) + "\n"));
    socket.on("data", chunk => {
      answer += chunk;
      const end = answer.indexOf("\n");
      if (end >= 0) { socket.destroy(); resolve(JSON.parse(answer.slice(0, end))); }
    });
    socket.once("error", reject);
  });
}

async function startErisOverlayNetwork(binary, relay) {
  const data = path.join(scratch, "eon-data");
  const runtime = path.join(scratch, "eon-runtime");
  fs.mkdirSync(data, { mode: 0o700 });
  fs.mkdirSync(runtime, { mode: 0o700 });
  eon = { binary, env: { XDG_DATA_HOME: data, XDG_RUNTIME_DIR: runtime, ERIS_NETWORK: relay }, runtime, socket: path.join(runtime, "eon/agent.sock") };
  run(binary, ["serve", "--no-notify"], eon.env);
  await until("ErisOverlayNetwork listens", () => fs.existsSync(path.join(runtime, "eon/page")) && say({ op: "state" }));
  const made = await say({ op: "init", name: "laptop" });
  if (!made.ok) throw new Error("eon init: " + JSON.stringify(made));
}

// ---------------------------------------------------------------- ErisDB

let erisdb;

function coreEnv(relay, database) {
  return { DATABASE_URL: database, ERISDB_SECRET: SECRET, ERISDB_LISTEN: CORE, ERIS_NETWORK: relay, XDG_RUNTIME_DIR: eon.runtime };
}

/** The mesh's database as the device has it: the servers of `database/1`, the one in use first. */
function database(state) {
  return state.mesh?.servers?.find(server => server.capability === DATABASE);
}

/** `erisdb join`, approved by the person at the device. */
async function join() {
  const joining = run(erisdb.binary, ["join"], erisdb.env, { stdio: ["ignore", "pipe", "pipe"] });
  const done = finish(joining);
  const prompt = await until("ErisOverlayNetwork asks the person about ErisDB", async () =>
    (await say({ op: "state" })).approvals.find(a => a.serves.some(served => served.capability === DATABASE)));
  const decided = await say({ op: "decide", id: prompt.id });
  if (!decided.ok) throw new Error("approving ErisDB: " + JSON.stringify(decided));
  await done;
}

async function setPassphrase() {
  const setting = run(erisdb.binary, ["passphrase", "set"], erisdb.env, { stdio: ["pipe", "pipe", "pipe"] });
  await finish(setting, PASSPHRASE + "\n");
}

async function startCore() {
  core = run(erisdb.binary, ["serve"], erisdb.env);
  await until("ErisDB answers over HTTP", async () => (await fetch(`http://${CORE}/v1/health`)).ok);
  await until("the device reaches ErisDB", async () => database(await say({ op: "state" }))?.apps[0]?.reachable === true);
}

async function stopCore() {
  if (!core) return;
  const stopped = new Promise(resolve => core.once("exit", resolve));
  core.kill("SIGTERM");
  await stopped;
  core = null;
}

// ---------------------------------------------------------------- the apps

const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".wasm": "application/wasm" };

async function body(request) {
  let text = "";
  for await (const chunk of request) text += chunk;
  return text ? JSON.parse(text) : null;
}

async function harness(request, response, route) {
  const reply = (value, status = 200) => {
    response.writeHead(status, { "Content-Type": "application/json" }).end(JSON.stringify(value));
  };
  if (route === "/harness" && request.method === "GET") {
    return reply({ core: `http://${CORE}`, token: erisdb.token, passphrase: PASSPHRASE, relay: erisdb.env.ERIS_NETWORK });
  }
  if (route === "/harness/eon" && request.method === "POST") return reply(await say(await body(request)));
  if (route === "/harness/erisdb/stop" && request.method === "POST") { await stopCore(); return reply({ ok: true }); }
  if (route === "/harness/erisdb/start" && request.method === "POST") { await startCore(); return reply({ ok: true }); }
  reply({ error: "no such harness route" }, 404);
}

async function serve(request, response) {
  const route = new URL(request.url, "http://localhost").pathname;
  if (route === "/ready") { response.end("ready"); return; }
  if (route === "/harness" || route.startsWith("/harness/")) {
    try { await harness(request, response, route); }
    catch (error) { response.writeHead(500).end(String(error?.stack ?? error)); }
    return;
  }
  let resource = path.resolve(apps, "." + route);
  if (!resource.startsWith(apps + path.sep)) { response.writeHead(404).end(); return; }
  if (route.endsWith("/")) resource = path.join(resource, "index.html");
  try {
    const bytes = await readFile(resource);
    response.writeHead(200, { "Content-Type": TYPES[path.extname(resource)] ?? "application/octet-stream" }).end(bytes);
  } catch { response.writeHead(404).end(); }
}

// ---------------------------------------------------------------- start

(async () => {
  scratch = await mkdtemp(path.join(os.tmpdir(), "erisdb-browser-"));

  const relayBinary = process.env.ERISDB_RELAY_BIN || path.join(target, "debug/erisdb-test-relay");
  if (!process.env.ERISDB_RELAY_BIN) cargo(["build", "--manifest-path", path.join(__dirname, "relay/Cargo.toml")], { CARGO_TARGET_DIR: target });
  const coreBinary = process.env.ERISDB_BIN || path.join(root, "erisdb/target/debug/erisdb");
  if (!process.env.ERISDB_BIN) cargo(["build", "--manifest-path", path.join(root, "erisdb/Cargo.toml"), "--bin", "erisdb"]);
  const eonBinary = process.env.EON_BIN || path.join(target, "debug/eon");
  if (!process.env.EON_BIN) {
    if (!fs.existsSync(path.join(eonSource, "eon/Cargo.toml"))) {
      throw new Error("the suite runs ErisOverlayNetwork: set EON_SOURCE to its checkout or EON_BIN to a built eon");
    }
    cargo(["build", "--manifest-path", path.join(eonSource, "eon/Cargo.toml"), "--bin", "eon"], { CARGO_TARGET_DIR: target });
  }

  const relayProcess = run(relayBinary, [], {}, { stdio: ["ignore", "pipe", "inherit"] });
  const relay = await new Promise((resolve, reject) => {
    let said = "";
    relayProcess.stdout.on("data", chunk => {
      said += chunk;
      const line = said.split("\n")[0];
      if (said.includes("\n")) resolve(line.trim());
    });
    relayProcess.once("exit", code => reject(new Error(`the relay exited ${code}`)));
  });
  log("relay at", relay);

  postgres = execFileSync("docker", ["run", "--rm", "-d", "--label", "dev.erisdb.browser-suite=1", "-e", "POSTGRES_PASSWORD=erisdb-e2e", "-p", "127.0.0.1::5432", "postgres:17-alpine"], { encoding: "utf8" }).trim();
  const address = execFileSync("docker", ["port", postgres, "5432/tcp"], { encoding: "utf8" }).trim().split("\n")[0];
  const database = `postgres://postgres:erisdb-e2e@${address}/postgres`;
  await until("PostgreSQL is ready", () => {
    execFileSync("docker", ["exec", postgres, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"], { stdio: "ignore" });
    return true;
  });

  await startErisOverlayNetwork(eonBinary, relay);
  erisdb = { binary: coreBinary, env: coreEnv(relay, database) };
  await join();
  await setPassphrase();
  await startCore();
  erisdb.token = execFileSync(coreBinary, ["mint", "--grant", "*", "--ttl", "86400", "--secret", SECRET], { encoding: "utf8" }).trim();
  log("ErisDB serves, joined to the mesh");

  http = createServer((request, response) => { serve(request, response); }).listen(PORT, "127.0.0.1");
})().catch(error => { console.error(error); stop(); process.exit(1); });
