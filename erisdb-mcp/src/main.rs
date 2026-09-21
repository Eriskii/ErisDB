//! An ErisDB client speaking MCP over stdio, built on rmcp (the official MCP
//! Rust SDK). Every tool is a thin wrapper over the core's HTTP API — the
//! server holds a capability token and a base URL, nothing else. Success
//! returns the API's JSON as text content; failure returns an `isError`
//! result carrying the status and body, so a tool error never kills the
//! session.
//!
//! What the token grants is the outer bound of what the tools can do, but
//! it is not the only bound. Three things a token alone cannot stop — a
//! model minting itself a credential, a model answering a pairing request
//! that exists so a *human* answers it, and a model overwriting a record
//! it only half read — are held back here as well, by [`Policy`] and by
//! making the dangerous field required rather than optional.

mod session;

use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, RwLock};
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
const SCAN_PAGE: &str = "1000";

/// The default ceiling on the lifetime of a token this server causes to
/// exist — minted or approved: a day.
const DEFAULT_MAX_MINT_TTL: i64 = 86_400;

/// What the operator allowed, beyond what the token allows.
#[derive(Clone)]
struct Policy {
    /// Minting returns a live credential as tool text, which puts a
    /// working token in the transcript, in the logs, and in every export
    /// of the conversation. Off unless `ERISDB_MCP_ALLOW_MINT=1`.
    allow_mint: bool,
    /// Approving a pairing hands a credential to a client the operator
    /// may never have seen. The approval step exists precisely so that a
    /// human performs it, which is what makes a photographed QR code
    /// worth nothing. Off unless `ERISDB_MCP_ALLOW_APPROVE=1`.
    ///
    /// Denying is not gated: it only ever takes authority away.
    allow_approve: bool,
    /// The longest lifetime a token this server mints or approves gets,
    /// whatever was asked for. `ERISDB_MCP_MAX_MINT_TTL` in seconds; a day
    /// by default.
    max_mint_ttl: i64,
}

impl Policy {
    fn from_env() -> Self {
        let on = |key: &str| std::env::var(key).is_ok_and(|v| v == "1");
        Self {
            allow_mint: on("ERISDB_MCP_ALLOW_MINT"),
            allow_approve: on("ERISDB_MCP_ALLOW_APPROVE"),
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
    http: reqwest::Client,
    base: String,
    token: Arc<RwLock<String>>,
    session: Option<Arc<session::Session>>,
    renewal: Arc<Mutex<()>>,
    policy: Policy,
}

/// One HTTP call against the core; `Ok` is any response (the status decides
/// success/error downstream), `Err` is a transport failure.
impl ErisDBMcp {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let token = self.token.read().await.clone();
        let first = self.call_with(&token, method.clone(), path, query, body).await?;
        if first.0 != 401 { return Ok(first); }
        let Some(session) = &self.session else { return Ok(first); };
        let _renewal = self.renewal.lock().await;
        if *self.token.read().await == token {
            *self.token.write().await = session.renew(&self.http).await.map_err(|e| format!("renewal: {e}"))?;
        }
        let token = self.token.read().await.clone();
        self.call_with(&token, method, path, query, body).await
    }

    async fn call_with(&self, token: &str, method: reqwest::Method, path: &str,
        query: &[(&str, String)], body: Option<&Value>) -> Result<(u16, Value), String> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("x-bezel-client", "erisdb-mcp");
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| format!("transport: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| format!("transport: {e}"))?;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok((status, value))
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
    /// enclosed by what this server's own token holds.
    grants: Vec<String>,
    /// Lifetime in seconds. Required, positive, and capped by the
    /// operator's ERISDB_MCP_MAX_MINT_TTL: a token minted here always
    /// expires.
    ttl_secs: i64,
    /// Signed user identity stamped into every write's source — attribution,
    /// not privilege.
    user: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PairingId {
    /// The pairing session's UUID, from list_pairings.
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct ApprovePairing {
    /// The pairing session's UUID, from list_pairings.
    id: String,
    /// Exactly which permissions to hand over. Required: read the
    /// session's "requested" list and name the ones that are actually
    /// needed. Approving a subset is one field, not a special case, and
    /// "*" is refused here — a master key is a human's keystroke.
    granted: Vec<String>,
    /// Lifetime of the token the approval issues, in seconds. Capped by
    /// ERISDB_MCP_MAX_MINT_TTL, which is also what it defaults to.
    ttl_secs: Option<i64>,
    /// Signed user identity the paired client writes as — attribution,
    /// not privilege.
    user: Option<String>,
}

#[tool_router]
impl ErisDBMcp {
    fn new(base: String, token: String, policy: Policy) -> Self {
        Self { http: session::http(), base, token: Arc::new(RwLock::new(token)),
            session: None, renewal: Arc::new(Mutex::new(())), policy }
    }

    #[tool(description = "List every registered facet (the tables of the store): name, strictness, and JSON Schema. Read this first to learn what data exists.")]
    async fn list_facets(&self) -> Result<CallToolResult, McpError> {
        let q = [("facet", FACET_FACET.to_string()), ("limit", "1000".into())];
        Self::result(self.call(reqwest::Method::GET, "/v1/items", &q, None).await)
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
        Self::result(self.call(reqwest::Method::GET, "/v1/items", &q, None).await)
    }

    #[tool(description = "Fetch a single item by id, with body, revision, timestamps, and the source that last wrote it.")]
    async fn get_item(&self, Parameters(p): Parameters<GetItem>) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::GET, &format!("/v1/items/{}", p.id), &[], None).await)
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
                let q = [("facet", FACET_FACET.to_string()), ("limit", SCAN_PAGE.into())];
                match self.call(reqwest::Method::GET, "/v1/items", &q, None).await {
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
            let q = [("facet", facet), ("limit", SCAN_PAGE.into())];
            let (status, body) = match self.call(reqwest::Method::GET, "/v1/items", &q, None).await {
                Ok(r) => r,
                Err(e) => return Self::result(Err(e)),
            };
            if status != 200 {
                // A token narrower than the scan set skips facets it can't read.
                continue;
            }
            scanned += 1;
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
        Self::result(self.call(reqwest::Method::POST, "/v1/items", &[], Some(&body)).await)
    }

    #[tool(description = "Replace an item's body WHOLE — every field you leave out is deleted, so read the item first. revision is required and comes from get_item or read_items; a stale one is a 409, meaning someone wrote while you were reading.")]
    async fn update_item(&self, Parameters(p): Parameters<UpdateItem>) -> Result<CallToolResult, McpError> {
        let body = json!({ "body": p.body, "revision": p.revision });
        Self::result(self.call(reqwest::Method::PUT, &format!("/v1/items/{}", p.id), &[], Some(&body)).await)
    }

    #[tool(description = "Delete an item by id. Pass confirm=false first for a dry run: it returns the item that would go and changes nothing. Only confirm=true deletes. History stays on the change feed either way, readable via item_history.")]
    async fn delete_item(&self, Parameters(p): Parameters<DeleteItem>) -> Result<CallToolResult, McpError> {
        let path = format!("/v1/items/{}", p.id);
        if !p.confirm {
            // Look before you leap: the same read the caller should have
            // done, handed back with nothing done to it.
            return match self.call(reqwest::Method::GET, &path, &[], None).await {
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
        Self::result(self.call(reqwest::Method::DELETE, &path, &[], None).await)
    }

    #[tool(description = "Every state an item has ever been in: one row per change with seq, op, timestamp, body snapshot, revision, and source. Works for deleted items too.")]
    async fn item_history(&self, Parameters(p): Parameters<GetItem>) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::GET, &format!("/v1/items/{}/history", p.id), &[], None).await)
    }

    #[tool(description = "Git-revert, not time travel: write the body snapshot from a past change (by seq, from item_history) as a NEW revision. revision is required and comes from get_item; a stale one is a 409.")]
    async fn revert_item(&self, Parameters(p): Parameters<RevertItem>) -> Result<CallToolResult, McpError> {
        let body = json!({ "seq": p.seq, "revision": p.revision });
        Self::result(self.call(reqwest::Method::POST, &format!("/v1/items/{}/revert", p.id), &[], Some(&body)).await)
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
        Self::result(self.call(reqwest::Method::GET, "/v1/changes", &q, None).await)
    }

    #[tool(description = "Mint a narrower capability token (requires meta:capabilities:mint on this server's own token, and ERISDB_MCP_ALLOW_MINT=1 on this server). The minted grants must be enclosed by ours. The token comes back as text, so only ask when it is meant to be read.")]
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

        let mut body = json!({ "grants": p.grants, "ttl_secs": ttl });
        if let Some(u) = p.user {
            body["user"] = json!(u);
        }
        match self.call(reqwest::Method::POST, "/v1/capabilities", &[], Some(&body)).await {
            Ok((status, mut minted)) if (200..300).contains(&status) => {
                // Say what was granted, not what was asked for: a request
                // over the ceiling gets the ceiling.
                minted["ttl_secs"] = json!(ttl);
                Self::result(Ok((status, minted)))
            }
            other => Self::result(other),
        }
    }

    // ------------------------------------------------------------ dashboard

    #[tool(description = "What this token holds: its grants, when it expires, when its refresh chain ends, and the user it writes as. Needs no permission, so it always answers — ask it first to learn what the rest of these tools can do.")]
    async fn my_permissions(&self) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::GET, "/v1/permissions", &[], None).await)
    }

    #[tool(description = "The core's own state: version, feed head, how many facets, items and changes it holds, and its limits. Needs meta:server:read.")]
    async fn server_state(&self) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::GET, "/v1/server", &[], None).await)
    }

    #[tool(description = "Pairing sessions, newest first: who is asking to pair, which permissions they asked for, and where each request stands (pending, requested, approved, denied). A session with status \"requested\" is waiting on an answer. Needs meta:pairing:read.")]
    async fn list_pairings(&self) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::GET, "/v1/pairings", &[], None).await)
    }

    #[tool(description = "One pairing session by id: the client's name and the exact permissions it requested. Read this before approving anything.")]
    async fn get_pairing(&self, Parameters(p): Parameters<PairingId>) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::GET, &format!("/v1/pairings/{}", p.id), &[], None).await)
    }

    #[tool(description = "Approve a pairing request, granting exactly the permissions in `granted` and no others (requires ERISDB_MCP_ALLOW_APPROVE=1 on this server). This hands a real credential to whoever redeemed the code, so read get_pairing first, grant the narrowest set that could work, and never grant \"*\".")]
    async fn approve_pairing(&self, Parameters(p): Parameters<ApprovePairing>) -> Result<CallToolResult, McpError> {
        // The approval IS the security boundary of pairing: a code grants
        // nothing until a human answers the prompt it raises. An agent
        // approving silently removes exactly the step the design is built
        // around, so the operator has to say so first.
        if !self.policy.allow_approve {
            return Self::refuse(
                "approving pairings is disabled on this server: an approval hands a working \
                 credential to whoever redeemed the code, and the approval step exists so that \
                 a human answers it. The operator enables it with ERISDB_MCP_ALLOW_APPROVE=1, or \
                 answers the request themselves with `erisdb pair`. Denying is always available.",
            );
        }
        if p.granted.is_empty() {
            return Self::refuse("granted is empty: to grant nothing, use deny_pairing");
        }
        // A bare `*` is a master key over the whole store, forever after.
        // Whatever the operator switched on, that one is a keystroke a
        // person makes while looking at the request.
        if p.granted.iter().any(|g| g == "*") {
            return Self::refuse(
                "\"*\" is a master key over the whole store and is not granted through this \
                 server. Name the permissions the client actually needs, or hand out a master \
                 key yourself with `erisdb mint` after reviewing the required authority.",
            );
        }
        // An approved token is a credential like a minted one, so the
        // operator's ceiling covers it — including when nothing was asked
        // for, which would otherwise take the core's week-long default.
        let ttl = p.ttl_secs.unwrap_or(self.policy.max_mint_ttl);
        if ttl <= 0 {
            return Self::refuse("ttl_secs must be positive: an approved token always expires");
        }
        let ttl = ttl.min(self.policy.max_mint_ttl);

        let mut body = json!({ "granted": p.granted, "ttl_secs": ttl });
        if let Some(u) = p.user {
            body["user"] = json!(u);
        }
        let path = format!("/v1/pairings/{}/approve", p.id);
        match self.call(reqwest::Method::POST, &path, &[], Some(&body)).await {
            Ok((status, mut approved)) if (200..300).contains(&status) => {
                approved["ttl_secs"] = json!(ttl);
                Self::result(Ok((status, approved)))
            }
            other => Self::result(other),
        }
    }

    #[tool(description = "Deny a pairing request: the client gets nothing and the session is spent. Needs no switch — denying only ever takes authority away. When a request is unexpected or asks for more than it should, this is the answer.")]
    async fn deny_pairing(&self, Parameters(p): Parameters<PairingId>) -> Result<CallToolResult, McpError> {
        let path = format!("/v1/pairings/{}/deny", p.id);
        Self::result(self.call(reqwest::Method::POST, &path, &[], Some(&json!({}))).await)
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
             token may do, and list_facets to see what exists."
                .into(),
        );
        info
    }
}

/// The capability this server holds.
///
/// `ERISDB_TOKEN_FILE` is the one to prefer: a path keeps the token out of
/// the MCP config, out of this process's environment, and under whatever
/// file permissions the operator chose. `ERISDB_TOKEN` is the direct way
/// and takes second place.
fn token_from_env() -> anyhow::Result<String> {
    if let Ok(path) = std::env::var("ERISDB_TOKEN_FILE") {
        let token = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading ERISDB_TOKEN_FILE ({path}): {e}"))?;
        let token = token.trim();
        if token.is_empty() {
            anyhow::bail!("ERISDB_TOKEN_FILE ({path}) is empty");
        }
        return Ok(token.to_string());
    }
    std::env::var("ERISDB_TOKEN").map_err(|_| {
        anyhow::anyhow!(
            "no capability: set ERISDB_TOKEN_FILE to a file holding one, or ERISDB_TOKEN to the \
             token itself (mint one: erisdb mint --grant tasks:read,tasks:create --ttl 86400)"
        )
    })
}

#[derive(Parser)]
#[command(version, about = "ErisDB MCP bridge; pair once, renew until revoked")]
struct Args {
    /// Advanced override for where this installation remembers its pairing.
    #[arg(long, env = "ERISDB_SESSION_FILE", global = true)]
    session_file: Option<PathBuf>,
    /// Remembered installation (default: default). Separate applications can use separate profiles.
    #[arg(long, env = "ERISDB_PROFILE", global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(clap::Subcommand)]
enum Action {
    /// Pair this connection interactively, using a pasted ticket.
    Pair {
        ticket: String,
        #[arg(long, env = "ERISDB_URL")] url: Option<String>,
        #[arg(long)] name: Option<String>,
        #[arg(long = "grant", value_delimiter = ',', required = true)] grants: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let profile = args.profile.as_deref().unwrap_or("default");
    if let Some(Action::Pair { ticket, url, name, grants }) = args.command {
        let path = session::path(args.session_file, profile)?;
        let name = name.unwrap_or_else(|| format!("erisdb-mcp ({profile})"));
        return session::pair(&ticket, url.as_deref(), &path, &name, &grants).await;
    }
    let manual = args.session_file.is_none() && args.profile.is_none() &&
        (std::env::var_os("ERISDB_TOKEN_FILE").is_some() || std::env::var_os("ERISDB_TOKEN").is_some());
    let bridge = if !manual {
        let path = session::path(args.session_file, profile)?;
        let session = session::Session::read(&path)?;
        let mut bridge = ErisDBMcp::new(session.url.clone(), session.token.clone(), Policy::from_env());
        bridge.session = Some(Arc::new(session));
        bridge
    } else {
        let base = session::base_url(&std::env::var("ERISDB_URL")
            .map_err(|_| anyhow::anyhow!("set ERISDB_SESSION_FILE, or ERISDB_URL and ERISDB_TOKEN_FILE"))?)?;
        ErisDBMcp::new(base, token_from_env()?, Policy::from_env())
    };
    let service = bridge.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
