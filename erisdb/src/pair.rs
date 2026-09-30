//! `erisdb pair`: the operator's side of the ceremony, driven over the
//! ordinary API. Showing the ticket and putting the request to the person
//! are [`erislogin::prompt`]'s; this module polls the core for the session
//! and sends the answer back.

use std::path::Path;

use anyhow::{Context, Result};
use erislogin::{prompt, qr, ticket::Ticket};

/// Run the pairing conversation to its end: show the code, wait for a
/// client to ask, put the request to the operator, and answer it.
pub async fn run(
    http: &reqwest::Client,
    base: &str,
    admin: &str,
    id: &str,
    ticket: &Ticket,
    token_ttl: i64,
    qr_output: Option<&Path>,
) -> Result<()> {
    let mut out = std::io::stdout();
    let encoded = ticket.encode(crate::APP)?;
    prompt::show(&mut out, "core", ticket, &encoded)?;
    if let Some(path) = qr_output {
        qr::write_png(&encoded, path, 8)?;
        println!("saved {}", path.display());
    }
    println!("Waiting for a client. Press s to save the code as a png, or ctrl-c to stop.");
    prompt::prompt(&mut out, "> ")?;

    let mut lines = prompt::stdin_lines();
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
    let session = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => anyhow::bail!("pairing cancelled"),
            line = lines.recv() => match line {
                Some(l) if l.trim().eq_ignore_ascii_case("s") => {
                    let path = qr::png_path(crate::APP, ticket.name.as_deref());
                    qr::write_png(&encoded, &path, 8)?;
                    println!("saved {}", path.display());
                    prompt::prompt(&mut out, "> ")?;
                }
                Some(_) => prompt::prompt(&mut out, "> ")?,
                None => anyhow::bail!("pairing cancelled: input closed"),
            },
            _ = poll.tick() => {
                let body: serde_json::Value = http
                    .get(format!("{base}/v1/pairings/{id}"))
                    .bearer_auth(admin)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                anyhow::ensure!(body["body"]["expires"].as_i64().is_some_and(|end| end > erislogin::now()), "pairing expired");
                match body["body"]["status"].as_str() {
                    Some("requested") => break body,
                    Some("pending") => {}
                    Some(other) => {
                        println!("\npairing is {other}; nothing to do.");
                        return Ok(());
                    }
                    None => anyhow::bail!("core returned an invalid pairing session"),
                }
            }
        }
    };

    let request = prompt::Request {
        client: session["body"]["client"].as_str().unwrap_or("an unnamed client").to_string(),
        fingerprint: session["body"]["fingerprint"].as_str().context("pairing has no verification code")?.to_string(),
        asked: serde_json::from_value(session["body"]["requested"].clone())
            .context("pairing has invalid requested permissions")?,
        expires: session["body"]["expires"].as_i64().context("pairing has no expiry")?,
    };
    prompt::drain(&mut lines);
    let granted = prompt::decide(&mut lines, &mut out, &request).await?;

    let (action, body) = match &granted {
        Some(grants) => ("approve", serde_json::json!({ "granted": grants, "ttl_secs": token_ttl })),
        None => ("deny", serde_json::json!({})),
    };
    let response = http
        .post(format!("{base}/v1/pairings/{id}/{action}"))
        .bearer_auth(admin)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        anyhow::bail!("{} refused ({status}): {body}", if granted.is_some() { "approval" } else { "denial" });
    }
    if let Some(granted) = granted {
        println!("\napproved:");
        for grant in &granted {
            println!("  {grant}");
        }
        println!("\nThe client collects its token now.");
    } else {
        println!("denied.");
    }
    Ok(())
}
