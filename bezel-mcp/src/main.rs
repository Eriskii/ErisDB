//! A bezel client speaking MCP over stdio, built on rmcp (the official MCP
//! Rust SDK). Every tool is a thin wrapper over the core's HTTP API — the
//! server holds a capability token and a base URL, nothing else. Success
//! returns the API's JSON as text content; failure returns an `isError`
//! result carrying the status and body, so a tool error never kills the
//! session.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

/// The meta-facet holding facet definitions; also the fallback scan set for
/// unscoped search.
const FACET_FACET: &str = "facet";

#[derive(Clone)]
struct BezelMcp {
    http: reqwest::Client,
    base: String,
    token: String,
}

/// One HTTP call against the core; `Ok` is any response (the status decides
/// success/error downstream), `Err` is a transport failure.
impl BezelMcp {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("x-bezel-client", "bezel-mcp");
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

    /// The current revision of an item — used when a caller omits `revision`
    /// and accepts last-writer-wins semantics.
    async fn current_revision(&self, id: &str) -> Result<i64, String> {
        let (status, body) = self
            .call(reqwest::Method::GET, &format!("/v1/items/{id}"), &[], None)
            .await?;
        if status != 200 {
            return Err(format!("HTTP {status}: {body}"));
        }
        body["revision"].as_i64().ok_or_else(|| "item carries no revision".into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReadItems {
    /// The facet to read, e.g. "tasks/v1" or "lists/v1".
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
    /// Case-insensitive substring matched against each item's JSON body.
    query: String,
    /// Restrict the search to one facet; omitted, every registered facet is scanned.
    facet: Option<String>,
    /// Max hits returned (default 50).
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateItem {
    /// The facet the item belongs to. Registering a new facet is itself a
    /// create in the meta-facet "facet" with a body like
    /// {"name": "notes/v1", "strict": false, "schema": {}}.
    facet: String,
    /// The item body; validated against the facet's JSON Schema when the
    /// facet is strict.
    body: Value,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateItem {
    /// The item's UUID.
    id: String,
    /// The full replacement body.
    body: Value,
    /// The revision this update is based on (optimistic concurrency; 409 on
    /// mismatch). Omitted, the current revision is fetched first —
    /// last-writer-wins.
    revision: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct RevertItem {
    /// The item's UUID.
    id: String,
    /// The change-feed seq whose snapshot to restore (see item_history).
    seq: i64,
    /// The revision the caller believes is current; fetched when omitted.
    revision: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct ReadChanges {
    /// Cursor: return changes with seq greater than this (default 0).
    since: Option<i64>,
    /// Restrict to one facet; omitted, the global feed (needs a wildcard token).
    facet: Option<String>,
    /// Max changes (server clamps to 1..=5000, default 500).
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct MintCapability {
    /// Facets the token covers, e.g. ["tasks/v1"] or ["*"].
    facets: Vec<String>,
    /// Verbs granted: any of "read", "write", "admin".
    verbs: Vec<String>,
    /// Lifetime in seconds; omitted, the token never expires.
    ttl_secs: Option<i64>,
    /// Signed user identity stamped into every write's source — attribution,
    /// not privilege.
    user: Option<String>,
}

#[tool_router]
impl BezelMcp {
    fn new(base: String, token: String) -> Self {
        Self { http: reqwest::Client::new(), base, token }
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

    #[tool(description = "Case-insensitive substring search over item bodies. Scoped to one facet, or across every registered facet when facet is omitted.")]
    async fn search_items(&self, Parameters(p): Parameters<SearchItems>) -> Result<CallToolResult, McpError> {
        let limit = p.limit.unwrap_or(50);
        let needle = p.query.to_lowercase();
        let facets: Vec<String> = match p.facet {
            Some(f) => vec![f],
            None => {
                let q = [("facet", FACET_FACET.to_string()), ("limit", "1000".into())];
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
        let mut hits: Vec<Value> = Vec::new();
        'facets: for facet in facets {
            let q = [("facet", facet), ("limit", "1000".into())];
            let (status, body) = match self.call(reqwest::Method::GET, "/v1/items", &q, None).await {
                Ok(r) => r,
                Err(e) => return Self::result(Err(e)),
            };
            if status != 200 {
                // A token narrower than the scan set skips facets it can't read.
                continue;
            }
            for item in body["items"].as_array().into_iter().flatten() {
                let haystack = item["body"].to_string().to_lowercase();
                if haystack.contains(&needle) || item["id"].as_str() == Some(p.query.as_str()) {
                    hits.push(item.clone());
                    if hits.len() >= limit {
                        break 'facets;
                    }
                }
            }
        }
        Self::result(Ok((200, json!({ "items": hits }))))
    }

    #[tool(description = "Create an item in a facet. To register a NEW facet, create an item in the meta-facet \"facet\" with body {name, strict, schema?} — but check list_facets first and reuse an existing facet when one fits; never register a duplicate or near-duplicate.")]
    async fn create_item(&self, Parameters(p): Parameters<CreateItem>) -> Result<CallToolResult, McpError> {
        let body = json!({ "facet": p.facet, "body": p.body });
        Self::result(self.call(reqwest::Method::POST, "/v1/items", &[], Some(&body)).await)
    }

    #[tool(description = "Replace an item's body. Pass revision for optimistic concurrency (409 on conflict); omit it to base the write on the current revision.")]
    async fn update_item(&self, Parameters(p): Parameters<UpdateItem>) -> Result<CallToolResult, McpError> {
        let revision = match p.revision {
            Some(r) => r,
            None => match self.current_revision(&p.id).await {
                Ok(r) => r,
                Err(e) => return Self::result(Err(e)),
            },
        };
        let body = json!({ "body": p.body, "revision": revision });
        Self::result(self.call(reqwest::Method::PUT, &format!("/v1/items/{}", p.id), &[], Some(&body)).await)
    }

    #[tool(description = "Delete an item by id. Its history stays on the change feed and remains readable via item_history.")]
    async fn delete_item(&self, Parameters(p): Parameters<GetItem>) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::DELETE, &format!("/v1/items/{}", p.id), &[], None).await)
    }

    #[tool(description = "Every state an item has ever been in: one row per change with seq, op, timestamp, body snapshot, revision, and source. Works for deleted items too.")]
    async fn item_history(&self, Parameters(p): Parameters<GetItem>) -> Result<CallToolResult, McpError> {
        Self::result(self.call(reqwest::Method::GET, &format!("/v1/items/{}/history", p.id), &[], None).await)
    }

    #[tool(description = "Git-revert, not time travel: write the body snapshot from a past change (by seq, from item_history) as a NEW revision.")]
    async fn revert_item(&self, Parameters(p): Parameters<RevertItem>) -> Result<CallToolResult, McpError> {
        let revision = match p.revision {
            Some(r) => r,
            None => match self.current_revision(&p.id).await {
                Ok(r) => r,
                Err(e) => return Self::result(Err(e)),
            },
        };
        let body = json!({ "seq": p.seq, "revision": revision });
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

    #[tool(description = "Mint a narrower capability token (requires the admin verb on this server's own token). The minted scope must be enclosed by ours.")]
    async fn mint_capability(&self, Parameters(p): Parameters<MintCapability>) -> Result<CallToolResult, McpError> {
        let mut body = json!({ "facets": p.facets, "verbs": p.verbs });
        if let Some(t) = p.ttl_secs {
            body["ttl_secs"] = json!(t);
        }
        if let Some(u) = p.user {
            body["user"] = json!(u);
        }
        Self::result(self.call(reqwest::Method::POST, "/v1/capabilities", &[], Some(&body)).await)
    }
}

#[tool_handler]
impl ServerHandler for BezelMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info = Implementation::new("bezel-mcp", env!("CARGO_PKG_VERSION"));
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "bezel is a personal data store: items live in facets (named, \
             optionally schema-validated collections), every write lands on a \
             durable change feed, and history is never lost. Start with \
             list_facets to see what exists."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::var("BEZEL_URL")
        .map_err(|_| anyhow::anyhow!("BEZEL_URL is not set (e.g. http://127.0.0.1:8080)"))?
        .trim_end_matches('/')
        .to_string();
    let token = std::env::var("BEZEL_TOKEN")
        .map_err(|_| anyhow::anyhow!("BEZEL_TOKEN is not set (mint one: bezel mint …)"))?;
    let service = BezelMcp::new(base, token)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
