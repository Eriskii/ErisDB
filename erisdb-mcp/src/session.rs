//! A locally owned installation credential. Only its S256 commitment reaches Postgres.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};

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
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
                bail!("{} contains credentials: chmod 600 before use", path.display());
            }
        }
        let mut session: Self = serde_json::from_slice(&std::fs::read(path)?)?;
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
/// Create-new prevents silently replacing another connection's installation identity.
pub async fn pair(ticket: &str, fallback_url: Option<&str>, path: &Path, name: &str, grants: &[String]) -> Result<()> {
    if grants.is_empty() { bail!("request at least one permission with --grant"); }
    let payload = ticket.trim().strip_prefix("bezel://pair/").context("expected an ErisDB pairing ticket")?;
    let ticket: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?;
    if ticket["v"] != 1 { bail!("unsupported ticket version"); }
    let url = base_url(ticket["url"].as_str().or(fallback_url).context("this ticket needs --url for the HTTPS endpoint")?)?;
    let code = ticket["token"].as_str().context("ticket omitted pairing capability")?;
    let mut entropy = [0u8; 32];
    getrandom::fill(&mut entropy).map_err(|error| anyhow::anyhow!("OS randomness: {error}"))?;
    let proof = URL_SAFE_NO_PAD.encode(entropy);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(proof.as_bytes()));
    let http = http();
    // Reserve the private file before asking the operator to approve anything.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| format!("create new session {}", path.display()))?;
    let result: Result<()> = async {
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
                    file.sync_all()?;
                    eprintln!("Paired. Session saved to {}", path.display());
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
    }.await;
    if result.is_err() { drop(file); let _ = std::fs::remove_file(path); }
    result
}
