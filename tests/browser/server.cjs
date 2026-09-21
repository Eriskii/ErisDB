// Real core process and disposable PostgreSQL; no intercepted API requests.
const { execFileSync, spawn } = require("node:child_process");
const { createServer } = require("node:http");
const { readFile } = require("node:fs/promises");
const path = require("node:path");
const root = path.resolve(__dirname, "../..");
const secret = "erisdb-browser-e2e-only";
const binary = process.env.ERISDB_BIN || path.join(root, "erisdb/target/debug/erisdb");
let postgres, core, http;
let stopping = false;

function stop() {
  if (stopping) return;
  stopping = true;
  http?.close();
  core?.kill("SIGTERM");
  if (postgres) execFileSync("docker", ["rm", "-f", postgres], { stdio: "ignore" });
}
process.once("SIGTERM", () => { stop(); process.exit(0); });
process.once("SIGINT", () => { stop(); process.exit(0); });
process.once("exit", stop);

(async () => {
  if (!process.env.ERISDB_BIN) execFileSync("cargo", ["build", "--locked", "--manifest-path", "erisdb/Cargo.toml", "--bin", "erisdb"], { cwd: root, stdio: "inherit" });
  postgres = execFileSync("docker", ["run", "--rm", "-d", "-e", "POSTGRES_PASSWORD=erisdb-e2e", "-p", "127.0.0.1::5432", "postgres:17-alpine"], { encoding: "utf8" }).trim();
  const address = execFileSync("docker", ["port", postgres, "5432/tcp"], { encoding: "utf8" }).trim();
  const database = `postgres://postgres:erisdb-e2e@${address}/postgres`;
  for (let tries = 0; ; tries++) {
    try { execFileSync("docker", ["exec", postgres, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"], { stdio: "ignore" }); break; }
    catch (error) { if (tries === 100) throw error; await new Promise(resolve => setTimeout(resolve, 100)); }
  }
  core = spawn(binary, ["serve", "--database-url", database, "--secret", secret, "--listen", "127.0.0.1:18771", ...(process.env.ERISDB_TEST_IROH ? [] : ["--no-iroh"])], { stdio: "inherit" });
  for (let tries = 0; ; tries++) {
    try { if ((await fetch("http://127.0.0.1:18771/v1/health")).ok) break; }
    catch {}
    if (tries === 600 || core.exitCode !== null) throw new Error("real core did not start");
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  http = createServer(async (request, response) => {
    if (request.url === "/ready") { response.end("ready"); return; }
    let resource = path.resolve(root, "." + new URL(request.url, "http://localhost").pathname);
    if (!resource.startsWith(path.join(root, "apps") + path.sep)) { response.writeHead(404).end(); return; }
    if (resource.endsWith(path.sep)) resource += "index.html";
    if (!path.extname(resource)) resource = path.join(resource, "index.html");
    try {
      const type = resource.endsWith(".js") ? "text/javascript" : "text/html";
      response.setHeader("Content-Type", type);
      response.end(await readFile(resource));
    } catch { response.writeHead(404).end(); }
  }).listen(18770, "127.0.0.1");
})().catch(error => { console.error(error); stop(); process.exit(1); });
