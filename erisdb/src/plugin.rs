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

use anyhow::{bail, ensure, Context};
use axum::body::{Body, Bytes};
use axum::http::header::{HeaderName, HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
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

struct Plugin {
    name: String,
    description: String,
    executable: PathBuf,
    args: Vec<String>,
    environment: BTreeMap<String, bool>,
    timeout: Duration,
    operations: BTreeMap<String, Operation>,
}

struct Operation {
    description: String,
    permission: String,
    request_schema: Value,
    validator: jsonschema::Validator,
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
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
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
            .ok_or_else(|| Error::PluginNotFound(plugin_name.to_string()))?;
        let operation = plugin.operations.get(operation_name).ok_or_else(|| {
            Error::PluginOperationNotFound {
                plugin: plugin_name.to_string(),
                operation: operation_name.to_string(),
            }
        })?;

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
        let validator = crate::schema::compile(&operation.request_schema)
            .with_context(|| format!("{}: operation {name:?} request_schema", path.display()))?;
        operations.insert(
            name,
            Operation {
                description: operation.description,
                permission: operation.permission,
                request_schema: operation.request_schema,
                validator,
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

/// A single supervisor owns the child, its pipes and its concurrency permit.
/// Cancellation and the deadline cover stdin, headers, body, stderr and exit.
async fn start_process(
    plugin: &Plugin,
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
                )))
            }
            None => {}
        }
    }

    let mut child = command
        .spawn()
        .map_err(|e| Error::PluginFailed(format!("starting plugin {:?}: {e}", plugin.name)))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (head_tx, head_rx) = oneshot::channel();
    let (tx, rx) = mpsc::channel(8);
    let plugin_name = plugin.name.clone();
    let timeout = plugin.timeout;

    tokio::spawn(async move {
        let mut head_tx = Some(head_tx);
        let mut diagnostics = Vec::new();
        let result = {
            let write_input = async {
                if let Err(error) = stdin.write_all(&payload).await {
                    tracing::warn!(plugin = plugin_name, %error, "plugin stdin failed");
                }
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            };
            let read_output = async {
                let head = read_response_head(&mut stdout).await?;
                head_tx
                    .take()
                    .expect("one response")
                    .send(Ok(head))
                    .map_err(|_| anyhow::anyhow!("client disconnected"))?;
                let mut buffer = [0; 16 * 1024];
                loop {
                    let count = stdout.read(&mut buffer).await?;
                    if count == 0 {
                        break;
                    }
                    tx.send(Ok(Bytes::copy_from_slice(&buffer[..count])))
                        .await
                        .map_err(|_| anyhow::anyhow!("client disconnected"))?;
                }
                let status = child.wait().await?;
                ensure!(status.success(), "exited with {status}");
                Ok::<_, anyhow::Error>(())
            };
            let read_stderr = async {
                let mut buffer = [0; 4096];
                while let Ok(count @ 1..) = stderr.read(&mut buffer).await {
                    let room = MAX_PLUGIN_STDERR_BYTES.saturating_sub(diagnostics.len());
                    diagnostics.extend_from_slice(&buffer[..count.min(room)]);
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::select! {
                _ = tx.closed() => Err(anyhow::anyhow!("client disconnected")),
                result = tokio::time::timeout(timeout, async {
                    tokio::try_join!(write_input, read_output, read_stderr).map(|_| ())
                }) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("timed out"))),
            }
        };
        // Reap before releasing capacity or reporting failure. No pipe task
        // survives this scope, even if a descendant kept a pipe open.
        if result.is_err() {
            let _ = child.kill().await;
        }
        let _ = child.wait().await;
        drop(permit);
        let stderr = String::from_utf8_lossy(&diagnostics);
        if let Err(error) = result {
            tracing::error!(plugin = plugin_name, %error, %stderr, "plugin invocation failed");
            let error = format!("plugin {plugin_name:?}: {error:#}");
            if let Some(head_tx) = head_tx {
                let _ = head_tx.send(Err(Error::PluginFailed(error)));
            } else {
                // Backpressure may delay delivery, but the process and its
                // permit have already been released.
                let _ = tx.send(Err(std::io::Error::other(error))).await;
            }
        } else if !stderr.is_empty() {
            tracing::debug!(plugin = plugin_name, %stderr, "plugin stderr");
        }
    });

    let head = head_rx
        .await
        .map_err(|e| Error::PluginFailed(format!("plugin response task failed: {e}")))??;
    Ok(head.map(|()| Body::from_stream(ReceiverStream::new(rx))))
}

async fn read_response_head(
    stdout: &mut BufReader<tokio::process::ChildStdout>,
) -> anyhow::Result<Response<()>> {
    let mut line = Vec::new();
    // Limit while reading: checking the size after read_line permits an
    // unbounded allocation if the plugin never writes a newline.
    let read = stdout
        .take((MAX_PLUGIN_HEADER_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .await
        .context("reading response header")?;
    ensure!(
        read > 0 && read <= MAX_PLUGIN_HEADER_BYTES,
        "returned no valid response header"
    );
    let head: PluginResponseHead =
        serde_json::from_slice(&line).context("returned an invalid response header")?;
    ensure!(
        head.protocol == PLUGIN_PROTOCOL,
        "returned protocol {}, expected {PLUGIN_PROTOCOL}",
        head.protocol
    );
    ensure!(
        (200..=599).contains(&head.status),
        "returned non-final status {}",
        head.status
    );
    let content_type =
        HeaderValue::from_str(&head.content_type).context("returned invalid content type")?;
    let mut response = Response::builder()
        .status(StatusCode::from_u16(head.status)?)
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, "no-store");
    for (name, value) in head.headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).context("returned invalid header name")?;
        if !forbidden_response_header(&name) {
            response = response.header(
                name,
                HeaderValue::from_str(&value).context("returned invalid header value")?,
            );
        }
    }
    Ok(response.body(())?)
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
