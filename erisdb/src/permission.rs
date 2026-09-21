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
//! See docs/permissions.md.

use crate::error::{Error, Result};

/// The one namespace the core owns. Facets may not reach into it.
pub const META: &str = "meta";

/// Namespaces reserved for the core's own facets, so a registration can
/// never mint itself a meta permission by choosing a name.
pub const RESERVED: [&str; 4] = ["meta", "facet", "system", "pair"];

/// The actions the core enforces on a facet. A grant may name others; the
/// core carries them and never requires them.
pub const ACTIONS: [&str; 4] = ["read", "create", "update", "delete"];

/// True when `grant` covers `required`.
///
/// Segment by segment. `*` matches exactly one segment, and as a grant's
/// final segment it matches every remaining segment — which is what makes
/// `*` mean everything and `meta:*` reach `meta:pairing:approve`, while
/// `*:read` stops at two segments and cannot.
///
/// The same function decides whether one grant encloses another: pass the
/// child as `required`. `*` is wild on the grant side and an ordinary token
/// on the required side, which is exactly subsumption — `tasks:read` does
/// not cover `tasks:*`, because `read` is not `*`.
pub fn covers(grant: &str, required: &str) -> bool {
    let mut grants = grant.split(':').peekable();
    let mut required = required.split(':');
    while let Some(segment) = grants.next() {
        let Some(other) = required.next() else { return false };
        if segment == "*" && grants.peek().is_none() {
            return true;
        }
        if segment != "*" && segment != other {
            return false;
        }
    }
    required.next().is_none()
}

/// True when any grant in the set covers `required`.
pub fn granted(grants: &[String], required: &str) -> bool {
    grants.iter().any(|g| covers(g, required))
}

/// True when every grant in `child` is subsumed by some grant in `parent`.
pub fn encloses(parent: &[String], child: &[String]) -> bool {
    child.iter().all(|c| granted(parent, c))
}

/// The permissions both authorities grant, including crossing wildcards
/// such as `tasks:*` and `*:read`, whose intersection is `tasks:read`.
pub fn intersection(left: &[String], right: &[String]) -> Vec<String> {
    fn pattern(left: &str, right: &str) -> Option<String> {
        let a: Vec<_> = left.split(':').collect();
        let b: Vec<_> = right.split(':').collect();
        let mut result = Vec::new();
        for i in 0..a.len().max(b.len()) {
            let (&x, &y) = (a.get(i)?, b.get(i)?);
            if x == "*" && i + 1 == a.len() {
                result.extend_from_slice(&b[i..]);
                return Some(result.join(":"));
            }
            if y == "*" && i + 1 == b.len() {
                result.extend_from_slice(&a[i..]);
                return Some(result.join(":"));
            }
            result.push(match (x, y) {
                ("*", _) => y,
                (_, "*") => x,
                _ if x == y => x,
                _ => return None,
            });
        }
        Some(result.join(":"))
    }
    left.iter().flat_map(|a| right.iter().filter_map(move |b| pattern(a, b)))
        .fold(Vec::new(), |mut grants, grant| {
            if !grants.contains(&grant) { grants.push(grant); }
            grants
        })
}

fn segment_ok(seg: &str, allow_star: bool) -> bool {
    if seg == "*" {
        return allow_star;
    }
    let mut chars = seg.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-')
}

/// A grant is segments of `[a-z0-9][a-z0-9._-]*`, or `*`.
pub fn check_grant(grant: &str) -> Result<()> {
    if grant.is_empty() {
        return Err(Error::BadRequest("a grant cannot be empty".into()));
    }
    if grant.split(':').all(|s| segment_ok(s, true)) {
        Ok(())
    } else {
        Err(Error::BadRequest(format!(
            "{grant:?} is not a permission: segments are [a-z0-9][a-z0-9._-]* or *, joined by :"
        )))
    }
}

/// A facet's name is one segment, is not reserved, and cannot wildcard —
/// otherwise a registration would be a way to name a meta permission.
pub fn check_facet_name(name: &str) -> Result<()> {
    if !segment_ok(name, false) {
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
