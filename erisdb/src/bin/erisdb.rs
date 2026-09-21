//! The erisdb CLI: `erisdb serve` runs a core replica; `erisdb mint` cuts tokens.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "erisdb", about = "Stateless personal data core.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a core replica: migrate the store, serve TCP and Iroh.
    Serve {
        /// Postgres connection string.
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        /// TCP listen address. Loopback by default: the TCP path carries
        /// bearer tokens in the clear, so anything else wants TLS in front.
        #[arg(long, env = "ERISDB_LISTEN", default_value = "127.0.0.1:7700")]
        listen: String,
        /// HMAC secret for capability tokens.
        #[arg(long, env = "ERISDB_SECRET", hide_env_values = true)]
        secret: String,
        /// Seed for the Iroh endpoint identity. Defaults to `--secret`, which
        /// ties the server's address to the token key; set it separately to
        /// rotate one without moving the other.
        #[arg(long, env = "ERISDB_IROH_SECRET", hide_env_values = true)]
        iroh_secret: Option<String>,
        /// Skip the Iroh endpoint and serve TCP only.
        #[arg(long)]
        no_iroh: bool,
        /// Directory of one-shot plugin JSON manifests. Executables and
        /// secrets are deployment configuration; plugin calls never touch
        /// the store.
        #[arg(long, env = "ERISDB_PLUGIN_DIR")]
        plugin_dir: Option<PathBuf>,
    },
    /// Print the iroh endpoint id a core with this secret serves under.
    EndpointId {
        #[arg(long, env = "ERISDB_SECRET", hide_env_values = true)]
        secret: String,
        /// The endpoint seed, if the deployment sets one separately.
        #[arg(long, env = "ERISDB_IROH_SECRET", hide_env_values = true)]
        iroh_secret: Option<String>,
    },
    /// Pair a client: show a code to scan, then answer what it asks for.
    ///
    /// The code grants nothing. A client redeems it, says who it is and
    /// which permissions it wants, and you decide — so a photographed
    /// screen is not a leaked capability.
    Pair {
        /// The core to run pairing against.
        #[arg(long, env = "ERISDB_URL", default_value = "http://127.0.0.1:7700")]
        url: String,
        /// A label for the human, so the app can name what it paired with.
        #[arg(long)]
        name: Option<String>,
        /// How long the code stays open, in seconds.
        #[arg(long, default_value_t = 600)]
        ttl: i64,
        /// Lifetime of the token an approval issues, in seconds.
        #[arg(long, default_value_t = 604_800)]
        token_ttl: i64,
        /// Also save the QR to this PNG file immediately.
        #[arg(long)]
        qr_output: Option<PathBuf>,
        /// Put this url in the ticket for clients that cannot speak QUIC.
        /// Browsers need it; phones do not. Defaults to `--url` when that
        /// is not loopback, since a phone cannot dial your localhost.
        #[arg(long)]
        client_url: Option<String>,
        /// Leave the iroh endpoint id out of the ticket.
        #[arg(long)]
        no_iroh: bool,
        #[arg(long, env = "ERISDB_SECRET", hide_env_values = true)]
        secret: String,
        #[arg(long, env = "ERISDB_IROH_SECRET", hide_env_values = true)]
        iroh_secret: Option<String>,
    },
    /// Inspect, change permissions, or revoke registered app installations.
    Clients {
        #[arg(long, env = "ERISDB_URL", default_value = "http://127.0.0.1:7700")]
        url: String,
        #[arg(long, env = "ERISDB_SECRET", hide_env_values = true)]
        secret: String,
        #[command(subcommand)]
        action: ClientCommand,
    },
    /// Mint a capability token from the shared secret.
    Mint {
        /// A permission this token grants, repeatable or comma-separated:
        /// `tasks:read`, `tasks:*`, `meta:facets:write`, `*`. Named
        /// deliberately: a wildcard is a master key and should be typed.
        #[arg(long = "grant", required = true, value_delimiter = ',')]
        grants: Vec<String>,
        /// Token lifetime in seconds.
        #[arg(long, conflicts_with = "no_expiry", required_unless_present = "no_expiry")]
        ttl: Option<i64>,
        /// How long the token may keep refreshing itself, in seconds.
        /// Defaults to 30 days. After that a human mints a new one — which
        /// is the only bound on a leaked token, so keep it short.
        #[arg(long, conflicts_with = "no_expiry")]
        max_ttl: Option<i64>,
        /// Cut a token that never expires and cannot be revoked short of
        /// rotating the secret. For daemons that must not fail at 3am.
        #[arg(long)]
        no_expiry: bool,
        /// Signed user identity the token writes as (attribution, not
        /// privilege). Shows up in every write's source.
        #[arg(long)]
        user: Option<String>,
        #[arg(long, env = "ERISDB_SECRET", hide_env_values = true)]
        secret: String,
    },
}

#[derive(Subcommand)]
enum ClientCommand {
    List,
    Show { id: uuid::Uuid },
    Permissions {
        id: uuid::Uuid,
        #[arg(long = "grant", required = true, value_delimiter = ',')]
        grants: Vec<String>,
    },
    Revoke { id: uuid::Uuid },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "erisdb=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Serve { database_url, listen, secret, iroh_secret, no_iroh, plugin_dir } => {
            let plugins = match plugin_dir {
                Some(dir) => erisdb::PluginRegistry::load_dir(&dir)
                    .with_context(|| format!("loading plugins from {}", dir.display()))?,
                None => erisdb::PluginRegistry::empty(),
            };
            tracing::info!(plugins = plugins.len(), "loaded one-shot plugins");
            let pool = PgPoolOptions::new()
                .max_connections(16)
                .connect(&database_url)
                .await
                .context("connecting to the store")?;
            erisdb::MIGRATOR.run(&pool).await.context("migrating the store")?;
            let secret = secret.into_bytes();
            let app = erisdb::app_with_plugins(pool, secret.clone(), plugins);

            let listener = tokio::net::TcpListener::bind(&listen).await?;
            let bound = listener.local_addr()?;
            tracing::info!("tcp: http://{bound}");
            if !bound.ip().is_loopback() {
                tracing::warn!(
                    "listening on {bound}, which is not loopback: capability tokens cross \
                     this socket in the clear. Put TLS in front of it, or serve over iroh."
                );
            }

            // Connect info feeds source.addr stamping on TCP writes.
            let tcp = app
                .clone()
                .into_make_service_with_connect_info::<std::net::SocketAddr>();
            if no_iroh {
                axum::serve(listener, tcp).await?;
            } else {
                let seed = iroh_secret.as_ref().map(String::as_bytes).unwrap_or(&secret);
                let ep = erisdb::net::endpoint(seed).await?;
                tracing::info!("iroh endpoint id: {}", ep.id());
                let addr = erisdb::net::advertised_addr(&ep).await?;
                tracing::info!("iroh addr: {addr:?}");
                tokio::select! {
                    r = axum::serve(listener, tcp) => r?,
                    r = erisdb::net::serve(ep, app) => r?,
                }
            }
        }
        Command::EndpointId { secret, iroh_secret } => {
            let seed = iroh_secret.as_deref().unwrap_or(&secret);
            println!("{}", erisdb::net::endpoint_id(seed.as_bytes()));
        }
        Command::Pair { url, name, ttl, token_ttl, qr_output, client_url, no_iroh, secret, iroh_secret } => {
            let base = url.trim_end_matches('/').to_string();
            let eid = (!no_iroh).then(|| {
                let seed = iroh_secret.as_deref().unwrap_or(&secret);
                erisdb::net::endpoint_id(seed.as_bytes()).to_string()
            });
            // Inspect the actual host: substring checks miss IPv6 and
            // 127/8 addresses, and misclassify names such as localhost.example.
            let endpoint = reqwest::Url::parse(&base).context("invalid core URL")?;
            let loopback = endpoint.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost") || host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
            });
            let ticket_url = client_url.or_else(|| (!loopback).then(|| base.clone()));
            if eid.is_none() && ticket_url.is_none() {
                anyhow::bail!("--no-iroh against a loopback core leaves the ticket with no address a client could dial; pass --client-url");
            }
            // The CLI holds the secret, so it signs itself the short-lived
            // token it needs to drive pairing over the ordinary API — the
            // same one a dashboard would use.
            let admin = erisdb::auth::mint(
                secret.as_bytes(),
                &["*"],
                Some(ttl.clamp(60, 3600) + 300),
                None,
            )?;

            let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
            let cut: serde_json::Value = http
                .post(format!("{base}/v1/pairings"))
                .bearer_auth(&admin)
                .json(&serde_json::json!({ "ttl_secs": ttl }))
                .send()
                .await
                .with_context(|| format!("reaching a core at {base} — is it running?"))?
                .error_for_status()?
                .json()
                .await
                .context("reading the pairing the core cut")?;
            let id = cut["id"].as_str().context("the core returned no pairing id")?.to_string();
            let code = cut["secret"].as_str().context("the core returned no code")?.to_string();

            let ticket = erisdb::ticket::Ticket::new(code, eid, ticket_url, name)?;
            if let Err(error) = erisdb::pair::run(&http, &base, &admin, &id, &ticket, token_ttl, qr_output.as_deref()).await {
                let _ = http.post(format!("{base}/v1/pairings/{id}/deny")).bearer_auth(&admin).send().await;
                return Err(error);
            }
        }
        Command::Clients { url, secret, action } => {
            let base = url.trim_end_matches('/');
            let token = erisdb::auth::mint(secret.as_bytes(), &["*"], Some(300), None)?;
            let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
            let request = match action {
                ClientCommand::List => http.get(format!("{base}/v1/clients")),
                ClientCommand::Show { id } => http.get(format!("{base}/v1/clients/{id}")),
                ClientCommand::Revoke { id } => http.post(format!("{base}/v1/clients/{id}/revoke")),
                ClientCommand::Permissions { id, grants } => {
                    let current: serde_json::Value = http.get(format!("{base}/v1/clients/{id}"))
                        .bearer_auth(&token).send().await?.error_for_status()?.json().await?;
                    http.put(format!("{base}/v1/clients/{id}"))
                        .json(&serde_json::json!({ "grants": grants, "revision": current["revision"] }))
                }
            };
            let response = request.bearer_auth(&token).send().await?;
            let status = response.status();
            let body: serde_json::Value = response.json().await?;
            anyhow::ensure!(status.is_success(), "client operation refused ({status}): {body}");
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        Command::Mint { grants, ttl, max_ttl, no_expiry, user, secret } => {
            let grants: Vec<&str> = grants.iter().map(String::as_str).collect();
            // `--ttl 0` reads like "forever" and means "already expired": the
            // token would carry exp == now and be refused on first use. Say so
            // rather than handing over something that cannot work.
            if ttl.is_some_and(|t| t <= 0) {
                anyhow::bail!("--ttl must be positive; for a token that never expires use --no-expiry");
            }
            let ttl = if no_expiry { None } else { ttl };
            let token =
                erisdb::auth::mint_chain(secret.as_bytes(), &grants, ttl, max_ttl, user.as_deref())?;
            println!("{token}");
        }
    }
    Ok(())
}
