//! The erisdb CLI: `erisdb join` takes the core into its owner's mesh,
//! `erisdb serve` runs a core replica, `erisdb passphrase` keeps the
//! enrollment passphrase, `erisdb clients` manages installations, and
//! `erisdb mint` cuts tokens.

use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use erisoverlaynetwork::agent::{self, Answer, Ask};
use erisoverlaynetwork::enroll::EnrollService;
use erisoverlaynetwork::mesh::{Config, Local, Node};
use erisoverlaynetwork::net::Network;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

#[derive(Parser)]
#[command(name = "erisdb", about = "Stateless personal data core.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The seeds of the core's identity: the token key, and the Iroh key when
/// the deployment keeps them apart.
#[derive(clap::Args)]
struct Secrets {
    /// HMAC secret for capability tokens.
    #[arg(long, env = "ERISDB_SECRET", hide_env_values = true)]
    secret: String,
    /// Seed for the Iroh endpoint identity. Defaults to `--secret`, which
    /// ties the server's key to the token key; set it separately to rotate
    /// one without moving the other.
    #[arg(long, env = "ERISDB_IROH_SECRET", hide_env_values = true)]
    iroh_secret: Option<String>,
}

impl Secrets {
    fn iroh_key(&self) -> iroh::SecretKey {
        erisdb::net::key(
            self.iroh_secret
                .as_deref()
                .unwrap_or(&self.secret)
                .as_bytes(),
        )
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run a core replica: migrate the store, join the mesh's node, serve
    /// TCP and Iroh. Refuses to start before `erisdb join`.
    Serve {
        /// Postgres connection string.
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        /// TCP listen address. Loopback by default: the TCP path carries
        /// bearer tokens in the clear, so anything else wants TLS in front.
        #[arg(long, env = "ERISDB_LISTEN", default_value = "127.0.0.1:7700")]
        listen: String,
        #[command(flatten)]
        secrets: Secrets,
        /// How the core reaches the mesh: `internet`, a relay URL, or
        /// `loopback` (tests).
        #[arg(long, env = "ERIS_NETWORK", default_value = "internet")]
        network: Network,
        /// Directory of one-shot plugin JSON manifests. Executables and
        /// secrets are deployment configuration; plugin calls never touch
        /// the store.
        #[arg(long, env = "ERISDB_PLUGIN_DIR")]
        plugin_dir: Option<PathBuf>,
        /// Seconds a device waits after its third wrong passphrase in a row;
        /// each wrong guess after that doubles the wait, up to a day.
        #[arg(long, env = "ERISDB_LOCKOUT_SECS", default_value_t = 60)]
        lockout_secs: u64,
    },
    /// Ask this machine's eon to approve the core as the mesh's database,
    /// and keep the approval in the store.
    Join {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        #[command(flatten)]
        secrets: Secrets,
        /// What the device shows the person, and the mesh calls this core.
        #[arg(long, default_value = "ErisDB")]
        name: String,
    },
    /// Set the enrollment passphrase, or unlock devices locked out by
    /// wrong guesses.
    Passphrase {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        #[command(subcommand)]
        action: PassphraseCommand,
    },
    /// Print the iroh endpoint id a core with this secret serves under.
    EndpointId {
        #[command(flatten)]
        secrets: Secrets,
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
        #[arg(
            long,
            conflicts_with = "no_expiry",
            required_unless_present = "no_expiry"
        )]
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
enum PassphraseCommand {
    /// Set the passphrase, replacing any there was. Typed at the terminal,
    /// or read as one line from standard input.
    Set,
    /// Clear one device's wrong guesses (its endpoint id), or every device's.
    Unlock { device: Option<String> },
}

#[derive(Subcommand)]
enum ClientCommand {
    List,
    Show {
        id: uuid::Uuid,
    },
    Permissions {
        id: uuid::Uuid,
        #[arg(long = "grant", required = true, value_delimiter = ',')]
        grants: Vec<String>,
    },
    Revoke {
        id: uuid::Uuid,
    },
}

async fn store(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(database_url)
        .await
        .context("connecting to the store")?;
    erisdb::MIGRATOR
        .run(&pool)
        .await
        .context("migrating the store")?;
    Ok(pool)
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
        Command::Serve {
            database_url,
            listen,
            secrets,
            network,
            plugin_dir,
            lockout_secs,
        } => {
            let plugins = match plugin_dir {
                Some(dir) => erisdb::PluginRegistry::load_dir(&dir)
                    .with_context(|| format!("loading plugins from {}", dir.display()))?,
                None => erisdb::PluginRegistry::empty(),
            };
            tracing::info!(plugins = plugins.len(), "loaded one-shot plugins");
            let pool = store(&database_url).await?;
            let key = secrets.iroh_key();
            let Some(note) = erisdb::mesh::joined(&pool).await? else {
                bail!("this ErisDB is in no mesh yet: run `erisdb join` where eon runs, and approve it there");
            };
            if note.app() != key.public() {
                bail!(
                    "the stored approval is for another key ({}); this secret makes {}. Run `erisdb join` again",
                    note.app(),
                    key.public()
                );
            }
            let endpoint = erisoverlaynetwork::net::endpoint(key, &network).await?;
            let node = Node::start(Config {
                endpoint: endpoint.clone(),
                network,
                local: Local::App(note),
                store: Arc::new(erisdb::mesh::PgStore::new(pool.clone())),
            })
            .await
            .context("starting the mesh node")?;
            let mesh = erisdb::mesh::Mesh::new(node.clone());
            let gate = erisdb::enroll::Gate::new(
                pool.clone(),
                std::time::Duration::from_secs(lockout_secs),
            );
            let enrolling = EnrollService::new(node.clone(), Arc::new(gate));
            tracing::info!(mesh = ?node.mesh(), "iroh endpoint id: {}", endpoint.id());

            let listener = tokio::net::TcpListener::bind(&listen).await?;
            let bound = listener.local_addr()?;
            // Plugins run beside the core, so they reach it on loopback when
            // it listens everywhere.
            let mut reachable = bound;
            if reachable.ip().is_unspecified() {
                reachable.set_ip(match bound {
                    std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                });
            }
            let app = erisdb::AppState::new(pool.clone(), secrets.secret.into_bytes())
                .with_plugins(plugins.reachable_at(&format!("http://{reachable}")))
                .with_mesh(mesh.clone())
                .router();

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
            tokio::spawn(erisdb::mesh::watch(mesh, pool));
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                r = axum::serve(listener, tcp) => r?,
                r = erisdb::net::serve(endpoint.clone(), app, node.protocol(), enrolling) => r?,
                // Told to stop, the core closes its endpoint, so that everyone connected to it
                // hears so at once rather than when their connections time out.
                _ = tokio::signal::ctrl_c() => {
                    node.shutdown().await;
                    endpoint.close().await;
                }
                _ = terminate.recv() => {
                    node.shutdown().await;
                    endpoint.close().await;
                }
                _ = node.closed() => bail!(
                    "the iroh endpoint closed by itself (its sockets failed, as after a network change it could \
                     not rebind to); start erisdb again"
                ),
            }
        }
        Command::Join {
            database_url,
            secrets,
            name,
        } => {
            let pool = store(&database_url).await?;
            let app = secrets.iroh_key().public();
            let Some(mut eon) = agent::connect().await? else {
                bail!("no eon is listening here ($XDG_RUNTIME_DIR/eon/agent.sock): run this where eon runs");
            };
            let ask = Ask {
                name,
                uses: Default::default(),
                serves: vec![erisdb::serves()],
                app,
            };
            println!("Asking eon to make {app} this mesh's database. Approve it on the device.");
            let answer = tokio::select! {
                answer = eon.approve(&ask) => answer?,
                _ = tokio::signal::ctrl_c() => bail!("cancelled; the request is withdrawn"),
            };
            match answer {
                Answer::Approved { note, snapshot } => {
                    erisdb::mesh::join(&pool, &note, &snapshot).await?;
                    let list = snapshot.list()?;
                    let device = list
                        .member(&note.device())
                        .map(|m| m.name.clone())
                        .unwrap_or_default();
                    println!(
                        "Joined mesh {}: {device} approved this ErisDB.",
                        snapshot.mesh
                    );
                }
                Answer::Denied(why) => bail!("eon said no: {why}"),
            }
        }
        Command::Passphrase {
            database_url,
            action,
        } => {
            let pool = store(&database_url).await?;
            match action {
                PassphraseCommand::Set => {
                    let passphrase = read_passphrase()?;
                    erisdb::passphrase::set(&pool, &passphrase).await?;
                    println!("Passphrase set. Devices type it to let apps into ErisDB.");
                }
                PassphraseCommand::Unlock { device } => {
                    let device = device
                        .map(|d| {
                            d.parse::<erisoverlaynetwork::DeviceId>()
                                .context("DEVICE is a 64-hex endpoint id")
                        })
                        .transpose()?;
                    let cleared = erisdb::passphrase::unlock(&pool, device.as_ref()).await?;
                    println!(
                        "Cleared the wrong guesses of {cleared} device{}.",
                        if cleared == 1 { "" } else { "s" }
                    );
                }
            }
        }
        Command::EndpointId { secrets } => {
            println!("{}", secrets.iroh_key().public());
        }
        Command::Clients {
            url,
            secret,
            action,
        } => {
            let base = url.trim_end_matches('/');
            let token = erisdb::auth::mint(secret.as_bytes(), &["*"], Some(300), None)?;
            let http = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let request = match action {
                ClientCommand::List => http.get(format!("{base}/v1/clients")),
                ClientCommand::Show { id } => http.get(format!("{base}/v1/clients/{id}")),
                ClientCommand::Revoke { id } => http.post(format!("{base}/v1/clients/{id}/revoke")),
                ClientCommand::Permissions { id, grants } => {
                    let current: serde_json::Value = http
                        .get(format!("{base}/v1/clients/{id}"))
                        .bearer_auth(&token)
                        .send()
                        .await?
                        .error_for_status()?
                        .json()
                        .await?;
                    http.put(format!("{base}/v1/clients/{id}")).json(
                        &serde_json::json!({ "grants": grants, "revision": current["revision"] }),
                    )
                }
            };
            let response = request.bearer_auth(&token).send().await?;
            let status = response.status();
            let body: serde_json::Value = response.json().await?;
            anyhow::ensure!(
                status.is_success(),
                "client operation refused ({status}): {body}"
            );
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        Command::Mint {
            grants,
            ttl,
            max_ttl,
            no_expiry,
            user,
            secret,
        } => {
            let grants: Vec<&str> = grants.iter().map(String::as_str).collect();
            // `--ttl 0` reads like "forever" and means "already expired": the
            // token would carry exp == now and be refused on first use. Say so
            // rather than handing over something that cannot work.
            if ttl.is_some_and(|t| t <= 0) {
                bail!("--ttl must be positive; for a token that never expires use --no-expiry");
            }
            let ttl = if no_expiry { None } else { ttl };
            let token = erisdb::auth::mint_chain(
                secret.as_bytes(),
                &grants,
                ttl,
                max_ttl,
                user.as_deref(),
            )?;
            println!("{token}");
        }
    }
    Ok(())
}

/// The new passphrase: typed twice without echo when standard input is a
/// terminal, or one line of standard input when it is not.
fn read_passphrase() -> Result<String> {
    if std::io::stdin().is_terminal() {
        // Standard input itself, not the controlling terminal: a launcher may
        // run this in a session of its own, which has none.
        let ask = |prompt: &str| {
            let config = rpassword::ConfigBuilder::new()
                .input_file_path("/dev/stdin")
                .output_file_path("/dev/stderr")
                .build();
            rpassword::prompt_password_with_config(prompt, config)
        };
        let first = ask("New passphrase: ")?;
        erisdb::passphrase::validate(&first)?;
        if ask("Again: ")? != first {
            bail!("the two did not match; nothing changed");
        }
        return Ok(first);
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
