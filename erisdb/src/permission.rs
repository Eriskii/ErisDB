//! Namespaced permissions.
//!
//! A required permission is concrete and computed from the request:
//! `tasks:create`, `meta:pairing:approve`. A grant is what a token holds and
//! may wildcard: `tasks:*`, `meta:*`, `*`.
//!
//! The core keeps no registry of valid permissions. A grant naming a facet
//! that does not exist is legal and matches nothing until it does, and a
//! grant the core has no meaning for is carried, delegated and enclosed like
//! any other — which is what lets a new client cost zero backend changes.
//!
//! Matching, enclosure, intersection and grant validation are
//! [`erislogin::permission`]'s. This module holds what is ErisDB's own: the
//! namespaces it reserves and the permission each facet operation requires.
//! See docs/permissions.md.

use crate::error::{Error, Result};

pub use erislogin::permission::{check_grant, covers, encloses, granted, intersection};

/// The one namespace the core owns. Facets may not reach into it.
pub const META: &str = "meta";

/// Namespaces reserved for the core's own facets, so a registration can
/// never mint itself a meta permission by choosing a name.
pub const RESERVED: [&str; 4] = ["meta", "facet", "system", "pair"];

/// The actions the core enforces on a facet. A grant may name others; the
/// core carries them and never requires them.
pub const ACTIONS: [&str; 4] = ["read", "create", "update", "delete"];

/// A facet's name is one segment, is not reserved, and cannot wildcard —
/// otherwise a registration would be a way to name a meta permission.
pub fn check_facet_name(name: &str) -> Result<()> {
    if !erislogin::permission::is_segment(name, false) {
        return Err(Error::BadRequest(format!(
            "facet name {name:?} must be [a-z0-9][a-z0-9._-]* with no ':' or '*'"
        )));
    }
    if RESERVED.contains(&name) {
        return Err(Error::BadRequest(format!("facet name {name:?} is reserved by the core")));
    }
    Ok(())
}

/// The default permission for a facet operation. Core facets use `meta:`.
/// Initializing a missing definition additionally accepts its NAME:create
/// grant; that insert-only exception is authorized by the create handler.
pub fn for_facet(facet: &str, action: &str) -> String {
    match facet {
        crate::store::FACET_FACET => format!("{META}:facets:{}", if action == "read" { "read" } else { "write" }),
        crate::store::SYSTEM_FACET => format!("{META}:system:read"),
        // Reading a pairing session is a dashboard's job; changing one is
        // an approver's. Collapsing both onto `read` would make a
        // read-only pairing view able to rewrite the session it displays.
        crate::store::PAIR_FACET => match action {
            "read" => format!("{META}:pairing:read"),
            "create" => format!("{META}:pairing:create"),
            _ => format!("{META}:pairing:approve"),
        },
        other => format!("{other}:{action}"),
    }
}

/// The core's own stateful facets are driven by their own endpoints. The
/// item routes refuse to write them, so holding a pairing permission is
/// never a way to hand-edit a session into some state the state machine
/// would not have produced.
pub fn core_managed(facet: &str) -> bool {
    matches!(facet, crate::store::SYSTEM_FACET | crate::store::PAIR_FACET)
}
