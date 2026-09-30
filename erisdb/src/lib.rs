//! erisdb — stateless personal data core.
//!
//! One Postgres store, N interchangeable core replicas, facet-scoped
//! capability tokens, and a durable change feed doubling as the event bus.
//! Served over plain TCP and over Iroh QUIC.
//!
//! Pairing tickets, QR codes, the operator's terminal prompt, token
//! signing, installation identities, fingerprints, key derivation and
//! permission matching come from [erislogin]; this crate supplies what is
//! ErisDB's own: the capability payload, the pairing state in Postgres,
//! and the permissions its facets require.

pub mod api;
pub mod auth;
pub mod error;
pub mod installation;
pub mod net;
pub mod pair;
pub mod permission;
pub mod plugin;
mod store;
mod schema;

pub use api::{app, app_with_plugins};
pub use plugin::PluginRegistry;

/// The name ErisDB's pairing tickets carry, as in `erisdb://pair/…`, and
/// that its saved QR codes are named for.
pub const APP: &str = "erisdb";

/// Embedded ErisDB schema; initializes a fresh store and checks it on restart.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
