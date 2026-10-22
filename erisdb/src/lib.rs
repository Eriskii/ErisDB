//! erisdb — stateless personal data core.
//!
//! One Postgres store, N interchangeable core replicas, facet-scoped
//! capabilities, and a durable change feed doubling as the event bus.
//! Served over plain TCP and over Iroh QUIC.
//!
//! ErisDB is an app in its owner's [mesh](erisoverlaynetwork): a device
//! approved it to serve `database/1`, the mesh's one database, which apps
//! get into with ErisDB's passphrase. An app asks a device to use
//! `database/1`; the device approves it, and enrolls it here with the
//! person's passphrase over the mesh library's
//! [`enroll`](erisoverlaynetwork::enroll) protocol. From then on the app is
//! known by its Iroh key alone. Token signing, key derivation and permission
//! matching are the mesh library's; this crate supplies what is ErisDB's
//! own: the capability payload, the installations and the passphrase in
//! Postgres, and the permissions its facets require.

pub mod api;
pub mod auth;
pub mod enroll;
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

/// What ErisDB serves in the mesh, and what apps ask a device to use.
pub const CAPABILITY: &str = "database/1";

/// How ErisDB serves [`CAPABILITY`]: a mesh has a single database, the most
/// recently approved, and an app using it also needs ErisDB's passphrase.
pub fn serves() -> erisoverlaynetwork::Serves {
    erisoverlaynetwork::Serves {
        capability: CAPABILITY.parse().expect("database/1 is a capability"),
        single: true,
        passphrase: true,
    }
}

/// Embedded ErisDB schema; initializes a fresh store and checks it on restart.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
