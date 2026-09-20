//! One invocation of OpenAI Chat Completions.
//!
//! The process reads the erisdb plugin protocol from stdin, forwards the
//! `input` object unchanged as the Chat Completions request body, writes one
//! response-header line, streams the upstream body, and exits.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROTOCOL: u32 = 1;

#[derive(Deserialize)]
struct Invocation {
    protocol: u32,
    plugin: String,
    operation: String,
    input: Value,
}

#[derive(Serialize)]
struct ResponseHead {
    protocol: u32,
    status: u16,
    content_type: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut bytes)
        .await
        .context("reading invocation")?;
    let invocation: Invocation = serde_json::from_slice(&bytes).context("parsing invocation")?;
    if invocation.protocol != PROTOCOL {
        bail!("plugin protocol {} is not supported", invocation.protocol);
    }
    if invocation.plugin != "openai" || invocation.operation != "chat.completions" {
        bail!(
            "unsupported operation {}.{}",
            invocation.plugin,
            invocation.operation
        );
    }

    let api_key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is not set")?;
    let base = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".into())
        .trim_end_matches('/')
        .to_string();
    let url = format!("{base}/chat/completions");

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("erisdb-plugin-openai/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building OpenAI client")?;
    let mut request = client
        .post(url)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .json(&invocation.input);
    if let Ok(organization) = std::env::var("OPENAI_ORGANIZATION") {
        request = request.header("OpenAI-Organization", organization);
    }
    if let Ok(project) = std::env::var("OPENAI_PROJECT") {
        request = request.header("OpenAI-Project", project);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            write_local_error(502, "upstream_unavailable", &e.to_string()).await?;
            return Ok(());
        }
    };

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let mut headers = BTreeMap::new();
    for name in [
        "x-request-id",
        "openai-processing-ms",
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
        "retry-after",
    ] {
        if let Some(value) = response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
        {
            headers.insert(name.to_string(), value.to_string());
        }
    }

    let mut stdout = tokio::io::stdout();
    write_head(
        &mut stdout,
        ResponseHead {
            protocol: PROTOCOL,
            status,
            content_type,
            headers,
        },
    )
    .await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        stdout
            .write_all(&chunk.context("reading OpenAI response")?)
            .await?;
        stdout.flush().await?;
    }
    Ok(())
}

async fn write_local_error(status: u16, code: &str, detail: &str) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    write_head(
        &mut stdout,
        ResponseHead {
            protocol: PROTOCOL,
            status,
            content_type: "application/json".into(),
            headers: BTreeMap::new(),
        },
    )
    .await?;
    stdout
        .write_all(&serde_json::to_vec(
            &json!({ "error": code, "detail": detail }),
        )?)
        .await?;
    stdout.flush().await?;
    Ok(())
}

async fn write_head(stdout: &mut tokio::io::Stdout, head: ResponseHead) -> Result<()> {
    stdout.write_all(&serde_json::to_vec(&head)?).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}
