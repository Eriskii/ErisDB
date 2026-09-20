//! One-shot executable plugins.
//!
//! A plugin is deployment configuration plus an executable, not a service.
//! Each call starts a fresh process, writes one invocation to stdin, streams
//! its stdout into the HTTP response, and waits for it to exit. No invocation
//! is recorded in Postgres and no plugin process survives the request.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use axum::body::{Body, Bytes};
use axum::http::header::{HeaderName, HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

use crate::auth::Capability;
use crate::error::{Error, Result};
use crate::permission;

pub const PLUGIN_PROTOCOL: u32 = 1;
pub const MAX_PLUGIN_PROCESSES: usize = 32;
pub const MAX_PLUGIN_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_PLUGIN_HEADER_BYTES: usize = 16 * 1024;
const MAX_PLUGIN_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const MAX_TIMEOUT_SECS: u64 = 3600;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginManifest {
    protocol: u32,
    name: String,
    #[serde(default)]
    description: String,
    executable: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    /// Environment variable names to copy into the otherwise-empty child
    /// environment. `true` means the variable must be present.
    #[serde(default)]
    environment: BTreeMap<String, bool>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    operations: BTreeMap<String, OperationManifest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationManifest {
    #[serde(default)]
    description: String,
    permission: String,
    request_schema: Value,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

#[derive(Clone)]
struct Plugin {
    name: String,
    description: String,
    executable: PathBuf,
    args: Vec<String>,
    environment: BTreeMap<String, bool>,
    timeout: Duration,
    operations: BTreeMap<String, Operation>,
}

#[derive(Clone)]
struct Operation {
    description: String,
    permission: String,
    request_schema: Value,
    validator: Arc<jsonschema::Validator>,
}

/// The deployment-derived plugin registry. It is immutable after startup and
/// therefore disposable: every replica rebuilds it from the same manifest
/// directory, and no runtime registration can be lost.
#[derive(Clone)]
pub struct PluginRegistry {
    plugins: Arc<BTreeMap<String, Plugin>>,
    slots: Arc<Semaphore>,
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

impl PluginRegistry {
    pub fn empty() -> Self {
        Self {
            plugins: Arc::new(BTreeMap::new()),
            slots: Arc::new(Semaphore::new(MAX_PLUGIN_PROCESSES)),
        }
    }

    /// Load every `*.json` manifest in a directory, in filename order.
    /// Manifests and executable paths are operator-controlled deployment
    /// configuration; none of this touches the store.
    pub fn load_dir(dir: &Path) -> anyhow::Result<Self> {
        let mut paths = std::fs::read_dir(dir)
            .with_context(|| format!("reading plugin directory {}", dir.display()))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        paths.sort();

        let mut plugins = BTreeMap::new();
        for path in paths {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading plugin manifest {}", path.display()))?;
            let manifest: PluginManifest = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing plugin manifest {}", path.display()))?;
            let plugin = compile_manifest(manifest, &path)?;
            if plugins.insert(plugin.name.clone(), plugin).is_some() {
                bail!(
                    "duplicate plugin name in {}: every plugin name must be unique",
                    dir.display()
                );
            }
        }

        Ok(Self {
            plugins: Arc::new(plugins),
            slots: Arc::new(Semaphore::new(MAX_PLUGIN_PROCESSES)),
        })
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Describe only operations this capability could actually invoke.
    pub fn visible_to(&self, cap: &Capability) -> Vec<PluginDescription> {
        self.plugins
            .values()
            .filter_map(|plugin| {
                let operations = plugin
                    .operations
                    .iter()
                    .filter(|(_, op)| cap.granted(&op.permission))
                    .map(|(name, op)| OperationDescription {
                        name: name.clone(),
                        description: op.description.clone(),
                        permission: op.permission.clone(),
                        request_schema: op.request_schema.clone(),
                    })
                    .collect::<Vec<_>>();
                (!operations.is_empty()).then(|| PluginDescription {
                    name: plugin.name.clone(),
                    description: plugin.description.clone(),
                    operations,
                })
            })
            .collect()
    }

    /// Validate and execute one call. This function performs no database
    /// operation: the returned body is a live stream from this invocation's
    /// stdout.
    pub async fn invoke(
        &self,
        cap: &Capability,
        plugin_name: &str,
        operation_name: &str,
        input: Value,
    ) -> Result<Response<Body>> {
        let plugin = self
            .plugins
            .get(plugin_name)
            .ok_or_else(|| Error::PluginNotFound(plugin_name.to_string()))?
            .clone();
        let operation = plugin
            .operations
            .get(operation_name)
            .ok_or_else(|| Error::PluginOperationNotFound {
                plugin: plugin_name.to_string(),
                operation: operation_name.to_string(),
            })?
            .clone();

        cap.require(&operation.permission)?;
        if let Err(e) = operation.validator.validate(&input) {
            return Err(Error::PluginSchemaViolation {
                plugin: plugin_name.to_string(),
                operation: operation_name.to_string(),
                detail: e.to_string(),
            });
        }

        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Unavailable)?;

        let invocation = Invocation {
            protocol: PLUGIN_PROTOCOL,
            plugin: plugin_name,
            operation: operation_name,
            input,
            context: InvocationContext {
                user: cap.user.as_deref(),
            },
        };
        let mut payload = serde_json::to_vec(&invocation)
            .map_err(|e| Error::Internal(format!("serializing plugin invocation: {e}")))?;
        payload.push(b'\n');

        start_process(plugin, payload, permit).await
    }
}

#[derive(Debug, Serialize)]
pub struct PluginDescription {
    name: String,
    description: String,
    operations: Vec<OperationDescription>,
}

#[derive(Debug, Serialize)]
struct OperationDescription {
    name: String,
    description: String,
    permission: String,
    request_schema: Value,
}

#[derive(Serialize)]
struct Invocation<'a> {
    protocol: u32,
    plugin: &'a str,
    operation: &'a str,
    input: Value,
    context: InvocationContext<'a>,
}

#[derive(Serialize)]
struct InvocationContext<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginResponseHead {
    protocol: u32,
    status: u16,
    content_type: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

fn compile_manifest(manifest: PluginManifest, path: &Path) -> anyhow::Result<Plugin> {
    if manifest.protocol != PLUGIN_PROTOCOL {
        bail!(
            "{} uses plugin protocol {}, but this core speaks {}",
            path.display(),
            manifest.protocol,
            PLUGIN_PROTOCOL
        );
    }
    permission::check_facet_name(&manifest.name)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if manifest.operations.is_empty() {
        bail!("{}: a plugin needs at least one operation", path.display());
    }
    if manifest.timeout_secs == 0 || manifest.timeout_secs > MAX_TIMEOUT_SECS {
        bail!(
            "{}: timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}",
            path.display()
        );
    }
    if !manifest.executable.is_absolute() {
        bail!("{}: executable must be an absolute path", path.display());
    }

    for name in manifest.environment.keys() {
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
        if !valid {
            bail!(
                "{}: invalid environment variable name {name:?}",
                path.display()
            );
        }
    }

    let mut operations = BTreeMap::new();
    for (name, operation) in manifest.operations {
        check_operation_name(&name)
            .with_context(|| format!("{}: operation {name:?}", path.display()))?;
        permission::check_grant(&operation.permission)
            .map_err(|e| anyhow::anyhow!("{}: operation {name:?}: {e}", path.display()))?;
        if operation.permission.contains('*') {
            bail!(
                "{}: operation {name:?} requires a wildcard permission",
                path.display()
            );
        }
        if operation.permission.split(':').next() != Some(manifest.name.as_str()) {
            bail!(
                "{}: operation {name:?} permission {:?} is outside the plugin namespace {:?}",
                path.display(),
                operation.permission,
                manifest.name
            );
        }
        guard_schema(&operation.request_schema)
            .with_context(|| format!("{}: operation {name:?} request_schema", path.display()))?;
        let validator =
            jsonschema::validator_for(&operation.request_schema).with_context(|| {
                format!(
                    "{}: operation {name:?} request_schema does not compile",
                    path.display()
                )
            })?;
        operations.insert(
            name,
            Operation {
                description: operation.description,
                permission: operation.permission,
                request_schema: operation.request_schema,
                validator: Arc::new(validator),
            },
        );
    }

    Ok(Plugin {
        name: manifest.name,
        description: manifest.description,
        executable: manifest.executable,
        args: manifest.args,
        environment: manifest.environment,
        timeout: Duration::from_secs(manifest.timeout_secs),
        operations,
    })
}

fn check_operation_name(name: &str) -> anyhow::Result<()> {
    if name.contains(':') || name.contains('*') {
        bail!("must be one lowercase permission segment");
    }
    permission::check_grant(&format!("plugin:{name}"))
        .map_err(|_| anyhow::anyhow!("must be [a-z0-9][a-z0-9._-]*"))
}

/// Match facet-schema safety: plugin schemas are deployment data, but they
/// still must never make request validation fetch a URL or read a file.
fn guard_schema(schema: &Value) -> anyhow::Result<()> {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "$ref" | "$recursiveRef" | "$dynamicRef") {
                    let target = value
                        .as_str()
                        .with_context(|| format!("schema {key} must be a string"))?;
                    if !target.starts_with('#') {
                        bail!("schema {key} {target:?} points outside the document");
                    }
                }
                guard_schema(value)?;
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(guard_schema),
        _ => Ok(()),
    }
}

async fn start_process(
    plugin: Plugin,
    payload: Vec<u8>,
    permit: OwnedSemaphorePermit,
) -> Result<Response<Body>> {
    let mut command = Command::new(&plugin.executable);
    command
        .args(&plugin.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();

    for (name, required) in &plugin.environment {
        match std::env::var_os(name) {
            Some(value) => {
                command.env(name, value);
            }
            None if *required => {
                return Err(Error::PluginUnavailable(format!(
                    "plugin {:?} requires environment variable {name}",
                    plugin.name
                )));
            }
            None => {}
        }
    }

    let mut child = command
        .spawn()
        .map_err(|e| Error::PluginFailed(format!("starting plugin {:?}: {e}", plugin.name)))?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        Error::PluginFailed(format!("plugin {:?} has no stdin pipe", plugin.name))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        Error::PluginFailed(format!("plugin {:?} has no stdout pipe", plugin.name))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        Error::PluginFailed(format!("plugin {:?} has no stderr pipe", plugin.name))
    })?;

    let writer = tokio::spawn(async move {
        stdin.write_all(&payload).await?;
        stdin.shutdown().await
    });
    let stderr_reader = tokio::spawn(drain_stderr(stderr));
    let deadline = Instant::now() + plugin.timeout;
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    let read = match tokio::time::timeout_at(deadline, stdout.read_line(&mut line)).await {
        Ok(result) => result.map_err(|e| {
            Error::PluginFailed(format!(
                "reading plugin {:?} response header: {e}",
                plugin.name
            ))
        })?,
        Err(_) => {
            stop_before_response(&mut child, writer, stderr_reader, &plugin.name, "timed out")
                .await;
            return Err(Error::PluginFailed(format!(
                "plugin {:?} timed out",
                plugin.name
            )));
        }
    };

    if read == 0 || line.len() > MAX_PLUGIN_HEADER_BYTES {
        stop_before_response(
            &mut child,
            writer,
            stderr_reader,
            &plugin.name,
            "returned no valid response header",
        )
        .await;
        return Err(Error::PluginFailed(format!(
            "plugin {:?} returned no valid response header",
            plugin.name
        )));
    }

    let head: PluginResponseHead = match serde_json::from_str(line.trim_end()) {
        Ok(head) => head,
        Err(e) => {
            stop_before_response(
                &mut child,
                writer,
                stderr_reader,
                &plugin.name,
                "returned an invalid response header",
            )
            .await;
            return Err(Error::PluginFailed(format!(
                "plugin {:?} returned an invalid response header: {e}",
                plugin.name
            )));
        }
    };
    if head.protocol != PLUGIN_PROTOCOL {
        stop_before_response(
            &mut child,
            writer,
            stderr_reader,
            &plugin.name,
            "returned the wrong protocol version",
        )
        .await;
        return Err(Error::PluginFailed(format!(
            "plugin {:?} returned protocol {}, expected {PLUGIN_PROTOCOL}",
            plugin.name, head.protocol
        )));
    }

    if !(200..=599).contains(&head.status) {
        return Err(Error::PluginFailed(format!(
            "plugin {:?} returned non-final status {}",
            plugin.name, head.status
        )));
    }
    let status = StatusCode::from_u16(head.status).map_err(|e| {
        Error::PluginFailed(format!(
            "plugin {:?} returned invalid status {}: {e}",
            plugin.name, head.status
        ))
    })?;
    let content_type = HeaderValue::from_str(&head.content_type).map_err(|e| {
        Error::PluginFailed(format!(
            "plugin {:?} returned invalid content type: {e}",
            plugin.name
        ))
    })?;

    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type);
    for (name, value) in head.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            Error::PluginFailed(format!(
                "plugin {:?} returned invalid header name: {e}",
                plugin.name
            ))
        })?;
        if forbidden_response_header(&name) {
            continue;
        }
        let value = HeaderValue::from_str(&value).map_err(|e| {
            Error::PluginFailed(format!(
                "plugin {:?} returned invalid header value: {e}",
                plugin.name
            ))
        })?;
        response = response.header(name, value);
    }
    response = response.header(CACHE_CONTROL, "no-store");

    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(8);
    let plugin_name = plugin.name.clone();
    tokio::spawn(async move {
        pump_body(
            &plugin_name,
            &mut child,
            &mut stdout,
            writer,
            stderr_reader,
            deadline,
            tx,
        )
        .await;
        drop(permit);
    });

    response
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .map_err(|e| Error::Internal(format!("building plugin response: {e}")))
}

fn forbidden_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "cache-control"
            | "connection"
            | "content-length"
            | "content-type"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "www-authenticate"
    )
}

async fn pump_body(
    plugin_name: &str,
    child: &mut Child,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    writer: tokio::task::JoinHandle<std::io::Result<()>>,
    stderr_reader: tokio::task::JoinHandle<String>,
    deadline: Instant,
    tx: mpsc::Sender<std::result::Result<Bytes, std::io::Error>>,
) {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut timed_out = false;
    loop {
        let read = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                timed_out = true;
                let _ = tx.send(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "plugin invocation timed out",
                ))).await;
                break;
            }
            result = stdout.read(&mut buffer) => result,
        };
        let count = match read {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                break;
            }
        };
        let chunk = Bytes::copy_from_slice(&buffer[..count]);
        let sent = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                timed_out = true;
                false
            }
            result = tx.send(Ok(chunk)) => result.is_ok(),
        };
        if !sent {
            break;
        }
    }

    if timed_out || tx.is_closed() {
        let _ = child.kill().await;
    }
    let status = child.wait().await;
    let write_result = writer.await;
    let stderr = stderr_reader
        .await
        .unwrap_or_else(|e| format!("stderr task failed: {e}"));
    match status {
        Ok(status) if status.success() && !timed_out => {
            if let Ok(Err(e)) = write_result {
                tracing::warn!(plugin = plugin_name, error = %e, "plugin stdin failed");
            }
            if !stderr.is_empty() {
                tracing::debug!(plugin = plugin_name, stderr, "plugin stderr");
            }
        }
        Ok(status) => {
            tracing::error!(plugin = plugin_name, %status, stderr, timed_out, "plugin invocation failed");
        }
        Err(e) => {
            tracing::error!(plugin = plugin_name, error = %e, stderr, timed_out, "waiting for plugin failed");
        }
    }
}

async fn stop_before_response(
    child: &mut Child,
    writer: tokio::task::JoinHandle<std::io::Result<()>>,
    stderr_reader: tokio::task::JoinHandle<String>,
    plugin_name: &str,
    reason: &str,
) {
    let _ = child.kill().await;
    let status = child.wait().await;
    let _ = writer.await;
    let stderr = stderr_reader
        .await
        .unwrap_or_else(|e| format!("stderr task failed: {e}"));
    tracing::error!(
        plugin = plugin_name,
        ?status,
        stderr,
        reason,
        "plugin failed before response"
    );
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr) -> String {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let room = MAX_PLUGIN_STDERR_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..count.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(executable: &str) -> PluginManifest {
        PluginManifest {
            protocol: PLUGIN_PROTOCOL,
            name: "echo".into(),
            description: "echo input".into(),
            executable: executable.into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            timeout_secs: 30,
            operations: BTreeMap::from([(
                "call".into(),
                OperationManifest {
                    description: "call echo".into(),
                    permission: "echo:call".into(),
                    request_schema: json!({
                        "type": "object",
                        "required": ["message"],
                        "properties": {"message": {"type": "string"}},
                        "additionalProperties": false
                    }),
                },
            )]),
        }
    }

    #[test]
    fn manifests_compile_their_operation_schemas() {
        let path = Path::new("test.json");
        let plugin = compile_manifest(manifest("/bin/echo"), path).expect("valid manifest");
        let operation = &plugin.operations["call"];
        assert!(operation.validator.is_valid(&json!({"message": "hello"})));
        assert!(!operation.validator.is_valid(&json!({"message": 3})));
    }

    #[test]
    fn manifests_cannot_claim_another_namespace_or_external_ref() {
        let path = Path::new("test.json");
        let mut wrong_permission = manifest("/bin/echo");
        wrong_permission
            .operations
            .get_mut("call")
            .unwrap()
            .permission = "imap:read".into();
        assert!(compile_manifest(wrong_permission, path).is_err());

        let mut external_ref = manifest("/bin/echo");
        external_ref
            .operations
            .get_mut("call")
            .unwrap()
            .request_schema = json!({"$ref": "https://example.com/schema.json"});
        assert!(compile_manifest(external_ref, path).is_err());
    }

    #[test]
    fn plugins_cannot_override_host_security_or_framing_headers() {
        for name in [
            "authorization",
            "cache-control",
            "content-length",
            "content-type",
            "set-cookie",
            "transfer-encoding",
            "www-authenticate",
        ] {
            assert!(forbidden_response_header(&HeaderName::from_static(name)));
        }
        assert!(!forbidden_response_header(&HeaderName::from_static(
            "x-request-id"
        )));
    }
}
