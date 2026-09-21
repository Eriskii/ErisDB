//! erisdb — stateless personal data core.
//!
//! One Postgres store, N interchangeable core replicas, facet-scoped
//! capability tokens, and a durable change feed doubling as the event bus.
//! Served over plain TCP and over Iroh QUIC.

pub mod api;
pub mod auth;
pub mod error;
pub mod installation;
pub mod net;
pub mod pair;
pub mod permission;
pub mod plugin;
pub mod ticket;

pub use api::{app, app_with_plugins};
pub use plugin::PluginRegistry;

/// Embedded ErisDB schema; initializes a fresh store and checks it on restart.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
