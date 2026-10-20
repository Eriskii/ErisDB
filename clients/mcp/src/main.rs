//! An ErisDB client speaking MCP over stdio, built on rmcp (the official MCP
//! Rust SDK). Every tool is a thin wrapper over the core's API, reached over
//! Iroh as an app its owner's mesh let in: the client holds its key and
//! where ErisDB is, nothing more. Success returns the API's JSON as text
//! content; failure returns an `isError` result carrying the status and
//! body, so a tool error never kills the session.
//!
//! What the person approved is the outer bound of what the tools can do,
//! but it is not the only bound. Two things grants alone cannot stop — a
//! model minting itself a credential, and a model overwriting a record it
//! only half read — are held back here as well, by [`Policy`] and by making
//! the dangerous field required rather than optional.

mod profile;

use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

/// The meta-facet holding facet definitions; also the fallback scan set for
/// unscoped search.
const FACET_FACET: &str = "facet";

/// The most hits one search returns, however large a limit is asked for.
const MAX_HITS: usize = 200;

/// The most facets an unscoped search sweeps. Each one is a full read of
/// up to [`SCAN_PAGE`] items whose bodies are then lowercased, so a store
/// with many facets is a great deal of work to hang on one tool call.
const MAX_FACET_SCAN: usize = 25;

/// Items read per facet in one search pass — the core's own ceiling.
const SCAN_PAGE: usize = 1000;

/// The default ceiling on the lifetime of a token this client mints: a day.
const DEFAULT_MAX_MINT_TTL: i64 = 86_400;

/// What the operator allowed, beyond what the grants allow.
#[derive(Clone)]
struct Policy {
    /// Minting returns a live credential as tool text, which puts a
    /// working token in the transcript, in the logs, and in every export
    /// of the conversation. Off unless `ERISDB_MCP_ALLOW_MINT=1`.
    allow_mint: bool,
    /// The longest lifetime a token this server mints gets, whatever was
    /// asked for. `ERISDB_MCP_MAX_MINT_TTL` in seconds; a day by default.
    max_mint_ttl: i64,
}

impl Policy {
    fn from_env() -> Self {
        Self {
            allow_mint: std::env::var("ERISDB_MCP_ALLOW_MINT").is_ok_and(|v| v == "1"),
            max_mint_ttl: std::env::var("ERISDB_MCP_MAX_MINT_TTL")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&t: &i64| t > 0)
                .unwrap_or(DEFAULT_MAX_MINT_TTL),
        }
    }
}

#[derive(Clone)]
struct ErisDBMcp {
    client: Arc<erisdb_client::Client>,
    policy: Policy,
}

/// Percent-encode a query value: everything outside the unreserved set.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// One call against the core over Iroh; `Ok` is any response (the status
/// decides success/error downstream), `Err` is a transport failure.
impl ErisDBMcp {
    async fn call(&self, method: &str, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<(u16, Value), String> {
        let mut path = path.to_string();
        for (n, (key, value)) in query.iter().enumerate() {
            path.push(if n == 0 { '?' } else { '&' });
            path.push_str(&format!("{key}={}", encode(value)));
        }
        self.client.request(method, &path, body.cloned()).await.map_err(|e| format!("transport: {e:#}"))
    }

    /// Map an API response onto an MCP tool result: 2xx is success with the
    /// body as pretty JSON, anything else is a tool error carrying status
    /// and body.
    fn result(outcome: Result<(u16, Value), String>) -> Result<CallToolResult, McpError> {
        Ok(match outcome {
            Ok((status, body)) if (200..300).contains(&status) => {
                let body = if body.is_null() { json!({ "ok": true }) } else { body };
                CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string_pretty(&body).expect("json serializes"),
                )])
            }
            Ok((status, body)) => {
                CallToolResult::error(vec![ContentBlock::text(format!("HTTP {status}: {body}"))])
            }
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
        })
    }

    /// A refusal from this server rather than from the core: the tool
    /// never reached the network.
    fn refuse(message: impl Into<String>) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(message.into())]))
    }

    /// A minted credential's response reports the effective lifetime after
    /// the local cap.
    async fn issue(&self, path: &str, body: Value) -> Result<CallToolResult, McpError> {
        let outcome = self.call("POST", path, &[], Some(&body)).await;
        Self::result(outcome.map(|(status, mut reply)| {
            if (200..300).contains(&status) { reply["ttl_secs"] = body["ttl_secs"].clone(); }
            (status, reply)
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReadItems {
    /// The facet to read, e.g. "tasks" or "lists".
    facet: String,
    /// RFC 3339 timestamp; only items updated after it are returned.
    updated_since: Option<String>,
    /// Max items (server clamps to 1..=1000, default 100).
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct GetItem {
    /// The item's UUID.
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SearchItems {
    /// Case-insensitive substring matched against each item's JSON body,
    /// or an exact item id.
    query: String,
    /// Restrict the search to one facet; omitted, every registered facet is scanned.
    facet: Option<String>,
    /// Max hits returned (default 50, clamped to 200).
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateItem {
    /// The facet the item belongs to. Registering a new facet is itself a
    /// create in the meta-facet "facet" with a body like
    /// {"name": "notes", "version": 1, "strict": false, "schema": {}}.
    /// The name is a permission namespace, so it carries no version — a
    /// schema version lives in "version" and grants survive it.
    facet: String,
    /// The item body; validated against the facet's JSON Schema when the
    /// facet is strict.
    body: Value,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateItem {
    /// The item's UUID.
    id: String,
    /// The full replacement body. Everything not in it is gone — read the
    /// item first and send it back whole.
    body: Value,
    /// The revision this update is based on, from get_item or read_items.
    /// A stale one is a 409, which is the point: it means someone else
    /// wrote while you were reading.
    revision: i64,
}

#[derive(Deserialize, JsonSchema)]
struct RevertItem {
    /// The item's UUID.
    id: String,
    /// The change-feed seq whose snapshot to restore (see item_history).
    seq: i64,
    /// The revision this revert is based on, from get_item. A stale one
    /// is a 409.
    revision: i64,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteItem {
    /// The item's UUID.
    id: String,
    /// True deletes. False is a dry run: it returns the item that would
    /// go and changes nothing.
    confirm: bool,
}

#[derive(Deserialize, JsonSchema)]
struct ReadChanges {
    /// Cursor: return changes with seq greater than this (default 0).
    since: Option<i64>,
    /// Restrict to one facet (needs read on it); omitted, the feed across
    /// every facet, which needs meta:feed:read.
    facet: Option<String>,
    /// Max changes (server clamps to 1..=5000, default 500).
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct MintCapability {
    /// The permissions the token grants: ["tasks:read", "tasks:create"],
    /// ["tasks:*"], ["meta:facets:write"], or ["*"] for everything. The
    /// actions on a facet are read, create, update and delete. Must be
    /// enclosed by what this app holds.
    grants: Vec<String>,
    /// Lifetime in seconds. Required, positive, and capped by the
    /// operator's ERISDB_MCP_MAX_MINT_TTL: a token minted here always
    /// expires.
    ttl_secs: i64,
    /// Signed user identity stamped into every write's source — attribution,
    /// not privilege.
    user: Option<String>,
}

#[tool_router]
impl ErisDBMcp {
    fn new(client: erisdb_client::Client, policy: Policy) -> Self {
        Self { client: Arc::new(client), policy }
    }

    #[tool(description = "List every registered facet (the tables of the store): name, strictness, and JSON Schema. Read this first to learn what data exists.")]
    async fn list_facets(&self) -> Result<CallToolResult, McpError> {
        let q = [("facet", FACET_FACET.to_string()), ("limit", "1000".into())];
        Self::result(self.call("GET", "/v1/items", &q, None).await)
    }

    #[tool(description = "Read items from one facet, newest-last, optionally filtered by updated_since. This is the 'read a table' tool.")]
    async fn read_items(&self, Parameters(p): Parameters<ReadItems>) -> Result<CallToolResult, McpError> {
        let mut q = vec![("facet", p.facet)];
        if let Some(t) = p.updated_since {
            q.push(("updated_since", t));
        }
        if let Some(l) = p.limit {
            q.push(("limit", l.to_string()));
        }
        Self::result(self.call("GET", "/v1/items", &q, None).await)
    }

    #[tool(description = "Fetch a single item by id, with body, revision, timestamps, and the source that last wrote it.")]
    async fn get_item(&self, Parameters(p): Parameters<GetItem>) -> Result<CallToolResult, McpError> {
        Self::result(self.call("GET", &format!("/v1/items/{}", p.id), &[], None).await)
    }

    #[tool(description = "Case-insensitive substring search over item bodies, or lookup by exact item id. Scoped to one facet, or across every registered facet when facet is omitted. Returns {items, scanned_facets, truncated}; truncated means the sweep stopped early and there may be more.")]
    async fn search_items(&self, Parameters(p): Parameters<SearchItems>) -> Result<CallToolResult, McpError> {
        // One tool call is not licence to read the whole store: the hit
        // count is capped and so is the fan-out.
        let limit = p.limit.unwrap_or(50).clamp(1, MAX_HITS);
        let needle = p.query.to_lowercase();
        let mut facets: Vec<String> = match p.facet {
            Some(f) => vec![f],
            None => {
                let q = [("facet", FACET_FACET.to_string()), ("limit", SCAN_PAGE.to_string())];
                match self.call("GET", "/v1/items", &q, None).await {
                    Err(e) => return Self::result(Err(e)),
                    Ok((status, body)) if status != 200 => {
                        return Self::result(Ok((status, body)));
                    }
                    Ok((_, body)) => {
                        let mut names: Vec<String> = body["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|i| i["body"]["name"].as_str().map(str::to_string))
                            .collect();
                        names.push(FACET_FACET.to_string());
                        names.sort();
                        names.dedup();
                        names
                    }
                }
            }
        };
        let mut truncated = facets.len() > MAX_FACET_SCAN;
        facets.truncate(MAX_FACET_SCAN);

        let mut hits: Vec<Value> = Vec::new();
        let mut scanned = 0usize;
        'facets: for facet in facets {
            let q = [("facet", facet), ("limit", SCAN_PAGE.to_string())];
            let (status, body) = match self.call("GET", "/v1/items", &q, None).await {
                Ok(r) => r,
                Err(e) => return Self::result(Err(e)),
            };
            if status == 403 {
                // Grants narrower than the scan set skip facets they cannot read.
                continue;
            }
            if status != 200 { return Self::result(Ok((status, body))); }
            scanned += 1;
            truncated |= body["items"].as_array().is_some_and(|items| items.len() == SCAN_PAGE);
            for item in body["items"].as_array().into_iter().flatten() {
                // Both halves of the match ignore case: an id typed in
                // lowercase is the same id as the one the store printed.
                let haystack = item["body"].to_string().to_lowercase();
                let by_id = item["id"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(&p.query));
                if haystack.contains(&needle) || by_id {
                    hits.push(item.clone());
                    if hits.len() >= limit {
                        truncated = true;
                        break 'facets;
                    }
                }
            }
        }
        Self::result(Ok((
            200,
            json!({ "items": hits, "scanned_facets": scanned, "truncated": truncated }),
        )))
    }

    #[tool(description = "Create an item in a facet. To register a NEW facet, create an item in the meta-facet \"facet\" with body {name, strict, schema?} — but check list_facets first and reuse an existing facet when one fits; never register a duplicate or near-duplicate.")]
    async fn create_item(&self, Parameters(p): Parameters<CreateItem>) -> Result<CallToolResult, McpError> {
        let body = json!({ "facet": p.facet, "body": p.body });
        Self::result(self.call("POST", "/v1/items", &[], Some(&body)).await)
    }

    #[tool(description = "Replace an item's body WHOLE — every field you leave out is deleted, so read the item first. revision is required and comes from get_item or read_items; a stale one is a 409, meaning someone wrote while you were reading.")]
    async fn update_item(&self, Parameters(p): Parameters<UpdateItem>) -> Result<CallToolResult, McpError> {
        let body = json!({ "body": p.body, "revision": p.revision });
        Self::result(self.call("PUT", &format!("/v1/items/{}", p.id), &[], Some(&body)).await)
    }

    #[tool(description = "Delete an item by id. Pass confirm=false first for a dry run: it returns the item that would go and changes nothing. Only confirm=true deletes. History stays on the change feed either way, readable via item_history.")]
    async fn delete_item(&self, Parameters(p): Parameters<DeleteItem>) -> Result<CallToolResult, McpError> {
        let path = format!("/v1/items/{}", p.id);
        if !p.confirm {
            // Look before you leap: the same read the caller should have
            // done, handed back with nothing done to it.
            return match self.call("GET", &path, &[], None).await {
                Ok((status, body)) if (200..300).contains(&status) => Self::result(Ok((
                    status,
                    json!({
                        "deleted": false,
                        "item": body,
                        "next": "call delete_item again with confirm=true to delete this item",
                    }),
                ))),
                other => Self::result(other),
            };
        }
        Self::result(self.call("DELETE", &path, &[], None).await)
    }

    #[tool(description = "Every state an item has ever been in: one row per change with seq, op, timestamp, body snapshot, revision, and source. Works for deleted items too.")]
    async fn item_history(&self, Parameters(p): Parameters<GetItem>) -> Result<CallToolResult, McpError> {
        Self::result(self.call("GET", &format!("/v1/items/{}/history", p.id), &[], None).await)
    }

    #[tool(description = "Git-revert, not time travel: write the body snapshot from a past change (by seq, from item_history) as a NEW revision. revision is required and comes from get_item; a stale one is a 409.")]
    async fn revert_item(&self, Parameters(p): Parameters<RevertItem>) -> Result<CallToolResult, McpError> {
        let body = json!({ "seq": p.seq, "revision": p.revision });
        Self::result(self.call("POST", &format!("/v1/items/{}/revert", p.id), &[], Some(&body)).await)
    }

    #[tool(description = "The durable, totally-ordered change feed: everything that happened, cursor-paged by seq. Returns {changes, next}; pass next back as since to page.")]
    async fn read_changes(&self, Parameters(p): Parameters<ReadChanges>) -> Result<CallToolResult, McpError> {
        let mut q = vec![("since", p.since.unwrap_or(0).to_string())];
        if let Some(f) = p.facet {
            q.push(("facet", f));
        }
        if let Some(l) = p.limit {
            q.push(("limit", l.to_string()));
        }
        Self::result(self.call("GET", "/v1/changes", &q, None).await)
    }

    #[tool(description = "Mint a narrower capability token (requires meta:capabilities:mint among this app's grants, and ERISDB_MCP_ALLOW_MINT=1 on this server). The minted grants must be enclosed by ours. The token comes back as text, so only ask when it is meant to be read.")]
    async fn mint_capability(&self, Parameters(p): Parameters<MintCapability>) -> Result<CallToolResult, McpError> {
        // The answer is a working credential in the transcript. That is a
        // decision for whoever runs this server, not for whoever is
        // talking to it.
        if !self.policy.allow_mint {
            return Self::refuse(
                "minting is disabled on this server: a minted token would be returned as text, \
                 into the transcript and every copy of it. The operator enables it with \
                 ERISDB_MCP_ALLOW_MINT=1.",
            );
        }
        if p.ttl_secs <= 0 {
            return Self::refuse("ttl_secs must be positive: a token minted here always expires");
        }
        let ttl = p.ttl_secs.min(self.policy.max_mint_ttl);

        self.issue("/v1/capabilities", json!({ "grants": p.grants, "ttl_secs": ttl, "user": p.user })).await
    }

    // ------------------------------------------------------------ dashboard

    #[tool(description = "What this app holds: its grants and the installation they belong to. Needs no permission, so it always answers — ask it first to learn what the rest of these tools can do.")]
    async fn my_permissions(&self) -> Result<CallToolResult, McpError> {
        Self::result(self.call("GET", "/v1/permissions", &[], None).await)
    }

    #[tool(description = "The core's own state: version, feed head, how many facets, items and changes it holds, and its limits. Needs meta:server:read.")]
    async fn server_state(&self) -> Result<CallToolResult, McpError> {
        Self::result(self.call("GET", "/v1/server", &[], None).await)
    }
}

#[tool_handler]
impl ServerHandler for ErisDBMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info = Implementation::new("erisdb-mcp", env!("CARGO_PKG_VERSION"));
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "erisdb is a personal data store: items live in facets (named, \
             optionally schema-validated collections), every write lands on a \
             durable change feed, and history is never lost. A facet's name is \
             also its permission namespace — tasks:read, tasks:create — so it \
             carries no version. Start with my_permissions to learn what this \
             app may do, and list_facets to see what exists."
                .into(),
        );
        info
    }
}

#[derive(Parser)]
#[command(version, about = "ErisDB MCP client; sign in once through ErisAuth, then serve MCP on stdio")]
struct Args {
    /// Advanced override for where this app keeps its key and ErisDB's address.
    #[arg(long, env = "ERISDB_PROFILE_FILE", global = true)]
    profile_file: Option<PathBuf>,
    /// The app this is (default: default). Separate applications use
    /// separate profiles: separate keys, grants and revocation.
    #[arg(long, env = "ERISDB_PROFILE", global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(clap::Subcommand)]
enum Action {
    /// Ask this machine's ErisAuth to let this app into ErisDB with these
    /// permissions; the person approves on ErisAuth's screen.
    SignIn {
        /// What ErisAuth shows the person. Default: erisdb-mcp (PROFILE).
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "grant", value_delimiter = ',', required = true)]
        grants: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let name = args.profile.as_deref().unwrap_or("default");
    let path = profile::path(args.profile_file, name)?;
    if let Some(Action::SignIn { name: shown, grants }) = args.command {
        let shown = shown.unwrap_or_else(|| format!("erisdb-mcp ({name})"));
        return profile::sign_in(&path, &shown, &grants).await;
    }
    let saved = profile::Profile::read(&path)?;
    let client = erisdb_client::Client::dial(saved.server.clone(), saved.secret()?, "erisdb-mcp").await?;
    let service = ErisDBMcp::new(client, Policy::from_env()).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
