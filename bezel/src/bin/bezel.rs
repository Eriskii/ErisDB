//! The bezel CLI: `bezel serve` runs a core replica; `bezel mint` cuts tokens.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;

#[derive(Parser)]
#[command(name = "bezel", about = "Stateless personal data core.")]
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
        /// TCP listen address.
        #[arg(long, default_value = "127.0.0.1:7700")]
        listen: String,
        /// HMAC secret for capability tokens.
        #[arg(long, env = "BEZEL_SECRET", hide_env_values = true)]
        secret: String,
        /// Skip the Iroh endpoint and serve TCP only.
        #[arg(long)]
        no_iroh: bool,
    },
    /// Mint a capability token from the shared secret.
    Mint {
        /// Comma-separated facet names, or `*`.
        #[arg(long, default_value = "*")]
        facets: String,
        /// Comma-separated verbs: read, write, admin.
        #[arg(long, default_value = "read,write")]
        verbs: String,
        /// Token lifetime in seconds; omit for a non-expiring token.
        #[arg(long)]
        ttl: Option<i64>,
        /// Signed user identity the token writes as (attribution, not
        /// privilege). Shows up in every write's source.
        #[arg(long)]
        user: Option<String>,
        #[arg(long, env = "BEZEL_SECRET", hide_env_values = true)]
        secret: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bezel=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Serve { database_url, listen, secret, no_iroh } => {
            let pool = PgPoolOptions::new()
                .max_connections(16)
                .connect(&database_url)
                .await
                .context("connecting to the store")?;
            bezel::MIGRATOR.run(&pool).await.context("migrating the store")?;
            let secret = secret.into_bytes();
            let app = bezel::app(pool, secret.clone());

            let listener = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!("tcp: http://{}", listener.local_addr()?);

            // Connect info feeds source.addr stamping on TCP writes.
            let tcp = app
                .clone()
                .into_make_service_with_connect_info::<std::net::SocketAddr>();
            if no_iroh {
                axum::serve(listener, tcp).await?;
            } else {
                let ep = bezel::net::endpoint(&secret).await?;
                tracing::info!("iroh endpoint id: {}", ep.id());
                let addr = bezel::net::advertised_addr(&ep).await?;
                tracing::info!("iroh addr: {addr:?}");
                tokio::select! {
                    r = axum::serve(listener, tcp) => r?,
                    r = bezel::net::serve(ep, app) => r?,
                }
            }
        }
        Command::Mint { facets, verbs, ttl, user, secret } => {
            let facets: Vec<&str> = facets.split(',').map(str::trim).collect();
            let verbs: Vec<&str> = verbs.split(',').map(str::trim).collect();
            let token = bezel::auth::mint(secret.as_bytes(), &facets, &verbs, ttl, user.as_deref())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{token}");
        }
    }
    Ok(())
}
