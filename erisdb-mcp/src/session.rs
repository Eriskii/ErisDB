//! A locally owned installation credential. Only its S256 commitment reaches Postgres.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Write, path::{Path, PathBuf}};

/// Profiles are names, not paths; normal setup never exposes credential storage.
pub fn path(explicit: Option<PathBuf>, profile: &str) -> Result<PathBuf> {
    if let Some(path) = explicit { return Ok(path); }
    if profile.is_empty() || profile.len() > 64 ||
        !profile.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)) {
        bail!("profile must contain 1–64 letters, digits, underscores or hyphens");
    }
    Ok(dirs::config_dir().context("cannot locate user configuration directory; use --session-file")?
        .join("erisdb/mcp").join(format!("{profile}.json")))
}

#[derive(Deserialize, Serialize)]
pub struct Session {
    pub url: String,
    pub client_id: String,
    pub token: String,
    pub refresh_secret: String,
}

pub fn base_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value).context("invalid core URL")?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || !url.username().is_empty() || url.password().is_some()
        || url.query().is_some() || url.fragment().is_some()
    { bail!("use HTTPS remotely, or HTTP on localhost, without URL credentials/query/fragment"); }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30)).build().expect("HTTP client")
}

impl Session {
    pub fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("no saved pairing at {}; run erisdb-mcp pair TICKET --grant PERMISSIONS first", path.display()))?;
        if !file.metadata()?.is_file() { bail!("session must be a regular file"); }
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o077 != 0 {
                bail!("{} contains credentials: chmod 600 before use", path.display());
            }
        }
        let mut session: Self = serde_json::from_reader(file)?;
        session.url = base_url(&session.url)?;
        Ok(session)
    }

    pub async fn renew(&self, http: &reqwest::Client) -> Result<String> {
        let reply = http.post(format!("{}/v1/clients/{}/refresh", self.url, self.client_id))
            .header("X-ErisDB-Client-Proof", &self.refresh_secret).json(&json!({}))
            .send().await?.error_for_status()?.json::<Value>().await?;
        Ok(reply["token"].as_str().context("renewal omitted token")?.to_owned())
    }
}

/// Runs outside MCP: the human compares the transcript fingerprint at both terminals.
/// Re-pairing reuses the proof and atomically replaces only this core's credential.
pub async fn pair(ticket: &str, fallback_url: Option<&str>, path: &Path, name: &str, grants: &[String]) -> Result<()> {
    if grants.is_empty() { bail!("request at least one permission with --grant"); }
    let payload = ticket.trim().strip_prefix("bezel://pair/").context("expected an ErisDB pairing ticket")?;
    let ticket: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?;
    if ticket["v"] != 1 { bail!("unsupported ticket version"); }
    let url = base_url(ticket["url"].as_str().or(fallback_url).context("this ticket needs --url for the HTTPS endpoint")?)?;
    let code = ticket["token"].as_str().context("ticket omitted pairing capability")?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)] {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(parent)?;
    // Keep the lock inode stable across atomic replacements and failed processes.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(path.with_extension("lock"))?;
    lock.try_lock().context("another pairing is already using this profile")?;
    let previous = match std::fs::symlink_metadata(path) {
        Ok(_) => Some(Session::read(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let proof = if let Some(previous) = &previous {
        if previous.url != url { bail!("this profile belongs to another core; choose a different --profile"); }
        previous.refresh_secret.clone()
    } else {
        let mut entropy = [0u8; 32];
        getrandom::fill(&mut entropy).map_err(|error| anyhow::anyhow!("OS randomness: {error}"))?;
        URL_SAFE_NO_PAD.encode(entropy)
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(proof.as_bytes()));
    let http = http();
    // Prove local storage is writable before requesting approval. On failure the
    // old session remains intact; readers see either the old or complete new one.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    let requested = http.post(format!("{url}/v1/pair/redeem")).bearer_auth(code)
        .json(&json!({"client": name, "requested": grants, "challenge": challenge}))
        .send().await?.error_for_status()?.json::<Value>().await?;
    eprintln!("Compare {} with the approving terminal; waiting for approval…",
        requested["body"]["fingerprint"].as_str().context("missing comparison fingerprint")?);
    loop {
        let reply = http.get(format!("{url}/v1/pair/status")).bearer_auth(code)
            .header("X-ErisDB-Client-Proof", &proof).send().await?
            .error_for_status()?.json::<Value>().await?;
        match reply["status"].as_str() {
            Some("approved") => {
                let session = Session {
                    url, refresh_secret: proof,
                    client_id: reply["client_id"].as_str().context("missing client id")?.to_owned(),
                    token: reply["token"].as_str().context("missing access token")?.to_owned(),
                };
                file.write_all(&serde_json::to_vec_pretty(&session)?)?;
                file.as_file().sync_all()?;
                if previous.is_some() { file.persist(path)?; }
                else { file.persist_noclobber(path)?; }
                #[cfg(unix)] std::fs::File::open(parent)?.sync_all()?;
                eprintln!("Paired. This connector will reconnect automatically.");
                return Ok(());
            }
            Some("denied") => bail!("pairing denied"),
            Some("requested" | "pending") => {}
            _ => bail!("unexpected pairing state"),
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => bail!("pairing cancelled"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}
