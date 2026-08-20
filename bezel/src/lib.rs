//! bezel — stateless personal data core.
//!
//! One Postgres store, N interchangeable core replicas, facet-scoped
//! capability tokens, and a durable change feed doubling as the event bus.
//! Served over plain TCP and over Iroh QUIC.

pub mod api;
pub mod auth;
pub mod error;
pub mod net;

pub use api::app;

/// Embedded schema migrations; run against a fresh or existing store.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
