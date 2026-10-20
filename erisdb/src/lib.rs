//! erisdb — stateless personal data core.
//!
//! One Postgres store, N interchangeable core replicas, facet-scoped
//! capabilities, and a durable change feed doubling as the event bus.
//! Served over plain TCP and over Iroh QUIC.
//!
//! ErisDB is an app in its owner's [mesh](erisoverlaynetwork): a device
//! approved it to serve `erisdb`, which makes it the mesh's ErisDB. Apps
//! get in through a device, which enrolls them here with the person's
//! passphrase, and from then on are known by their Iroh key alone. Token
//! signing, key derivation and permission matching are the mesh library's;
//! this crate supplies what is ErisDB's own: the capability payload, the
//! installations and the passphrase in Postgres, and the permissions its
//! facets require.

pub mod api;
pub mod auth;
pub mod error;
pub mod installation;
pub mod mesh;
pub mod net;
pub mod passphrase;
pub mod permission;
pub mod plugin;
mod schema;
mod store;

pub use api::{app, app_with_plugins, AppState};
pub use plugin::PluginRegistry;

/// The service ErisDB provides in the mesh, and that apps ask a device to
/// approve them for.
pub const SERVICE: &str = "erisdb";

/// Embedded ErisDB schema; initializes a fresh store and checks it on restart.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
