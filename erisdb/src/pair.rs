//! Presenting a pairing ticket to a human and a camera.
//!
//! A ticket is long — an endpoint id and a token — so the QR code is the
//! thing meant to be used, and the text underneath is the escape hatch for
//! when a camera is not available or a terminal renders blocks badly.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use qrcode::render::unicode::Dense1x2;
use qrcode::types::QrError;
use qrcode::QrCode;

/// Render a QR code as text, two module rows per line of output.
///
/// Half-block characters give square modules in a terminal, where cells are
/// about twice as tall as they are wide. A quiet zone of four modules is
/// part of the spec, not decoration: scanners need it to find the symbol.
pub fn to_blocks(data: &str) -> Result<String, QrError> {
    let mut out = QrCode::new(data)?.render::<Dense1x2>()
        .dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).build();
    out.push('\n');
    Ok(out)
}

/// The QR as a square greyscale raster: dark modules black, everything else
/// white, quiet zone included. `scale` is pixels per module. Returns the
/// side length and the pixels, one byte each.
pub fn raster(data: &str, scale: u32) -> Result<(u32, Vec<u8>)> {
    let code = QrCode::new(data).context("encoding the ticket as a QR code")?;
    let width = code.width();
    let modules = code.to_colors();
    const QUIET: u32 = 4;
    let side = (width as u32 + QUIET * 2) * scale;

    let mut pixels = vec![255u8; (side * side) as usize];
    for (i, color) in modules.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let mx = (i % width) as u32 + QUIET;
        let my = (i / width) as u32 + QUIET;
        for dy in 0..scale {
            let start = ((my * scale + dy) * side + mx * scale) as usize;
            pixels[start..start + scale as usize].fill(0);
        }
    }
    Ok((side, pixels))
}

/// Write the QR as a PNG, one file, no dependencies on a viewer. `scale` is
/// pixels per module; 8 is comfortable on a phone screen held at arm's
/// length.
pub fn write_png(data: &str, path: &Path, scale: u32) -> Result<()> {
    let (side, pixels) = raster(data, scale)?;
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), side, side);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing the png header")?;
    writer.write_image_data(&pixels).context("writing the png")?;
    Ok(())
}

/// Where a saved QR lands: the user's home directory, named for the core.
pub fn png_path(name: Option<&str>) -> PathBuf {
    let label = name.unwrap_or("erisdb");
    let safe: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(format!("erisdb-pair-{safe}.png"))
}

/// Print the ticket every way a person might want it: as a code for a
/// camera, and as text for anyone who would rather type.
pub fn show(ticket: &crate::ticket::Ticket, encoded: &str) {
    match to_blocks(encoded) {
        Ok(blocks) => println!("\n{blocks}"),
        Err(e) => {
            // Too much data for any QR version is the only realistic
            // failure, and it should not cost the operator the ticket.
            eprintln!("(the ticket does not fit in a QR code: {e})\n");
        }
    }
    println!("Scan this with the app, or with your phone's camera.\n");
    if let Some(name) = &ticket.name {
        println!("  core        {name}");
    }
    if let Some(eid) = &ticket.eid {
        println!("  endpoint id {eid}");
    }
    if let Some(url) = &ticket.url {
        println!("  url         {url}");
    }
    println!("  code        {}", ticket.token);
    println!("\n  ticket      {encoded}");
    println!("\nThe code grants nothing on its own — you approve what the client asks for.");
}

/// Lines the operator types, off the blocking stdin thread so polling and
/// typing can happen at once.
fn stdin_lines() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines().map_while(std::result::Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

fn prompt(text: &str) {
    print!("{text}");
    std::io::stdout().flush().ok();
}

async fn answer_before(lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>, expires: i64) -> Result<String> {
    let remaining = expires.saturating_sub(chrono::Utc::now().timestamp()).max(0) as u64;
    tokio::select! {
        line = lines.recv() => line.context("pairing cancelled: input closed"),
        _ = tokio::signal::ctrl_c() => anyhow::bail!("pairing cancelled"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(remaining)) => anyhow::bail!("pairing expired"),
    }
}

/// Run the pairing conversation to its end: show the code, wait for a
/// client to ask, put the request to the operator, and answer it.
pub async fn run(
    http: &reqwest::Client,
    base: &str,
    admin: &str,
    id: &str,
    ticket: &crate::ticket::Ticket,
    token_ttl: i64,
    qr_output: Option<&Path>,
) -> Result<()> {
    let encoded = ticket.encode()?;
    show(ticket, &encoded);
    if let Some(path) = qr_output {
        write_png(&encoded, path, 8)?;
        println!("saved {}", path.display());
    }
    println!("Waiting for a client. Press s to save the code as a png, or ctrl-c to stop.");
    prompt("> ");

    let mut lines = stdin_lines();
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
    let session = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => anyhow::bail!("pairing cancelled"),
            line = lines.recv() => match line {
                Some(l) if l.trim().eq_ignore_ascii_case("s") => {
                    let path = png_path(ticket.name.as_deref());
                    write_png(&encoded, &path, 8)?;
                    println!("saved {}", path.display());
                    prompt("> ");
                }
                Some(_) => prompt("> "),
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
                anyhow::ensure!(body["body"]["expires"].as_i64().is_some_and(|end| end > chrono::Utc::now().timestamp()), "pairing expired");
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

    let client = session["body"]["client"].as_str().unwrap_or("an unnamed client");
    let asked: Vec<String> = serde_json::from_value(session["body"]["requested"].clone())
        .context("pairing has invalid requested permissions")?;

    // Keystrokes entered while waiting are not an answer to a request the
    // operator has not seen. In particular, a queued newline must not deny it.
    while lines.try_recv().is_ok() {}
    let fingerprint = session["body"]["fingerprint"].as_str().context("pairing has no verification code")?;
    println!("\n\nCompare this code with the app: {fingerprint}");
    println!("Approve only if both displays match.\n{client} wants:");
    for (i, grant) in asked.iter().enumerate() {
        println!("  {}. {grant}", i + 1);
    }
    println!("\n[a] approve as asked   [s] select   [d] deny");
    prompt("> ");

    let expires = session["body"]["expires"].as_i64().context("pairing has no expiry")?;
    let granted = loop {
        let answer = answer_before(&mut lines, expires).await?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "a" => break Some(asked.clone()),
            "d" => break None,
            "s" => {
                let selected = loop {
                    println!("Numbers to approve, comma separated (empty denies):");
                    prompt("> ");
                    let picks = answer_before(&mut lines, expires).await?;
                    if picks.trim().is_empty() { break None; }
                    let grants: Option<Vec<String>> = picks.split(',').map(|p| {
                        let index = p.trim().parse::<usize>().ok()?.checked_sub(1)?;
                        asked.get(index).cloned()
                    }).collect();
                    if let Some(grants) = grants { break Some(grants); }
                    println!("Enter valid permission numbers from 1 to {}.", asked.len());
                };
                break selected;
            }
            _ => {
                println!("Enter a, s, or d.");
                prompt("> ");
            }
        }
    };

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
