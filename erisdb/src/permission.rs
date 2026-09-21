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
    let g: Vec<&str> = grant.split(':').collect();
    let r: Vec<&str> = required.split(':').collect();
    for (i, seg) in g.iter().enumerate() {
        let last = i + 1 == g.len();
        if *seg == "*" && last {
            // Trailing wildcard: the rest, and there must be a rest.
            return r.len() >= g.len();
        }
        match r.get(i) {
            None => return false,
            Some(other) => {
                if *seg != "*" && seg != other {
                    return false;
                }
            }
        }
    }
    r.len() == g.len()
}

/// True when any grant in the set covers `required`.
pub fn granted(grants: &[String], required: &str) -> bool {
    grants.iter().any(|g| covers(g, required))
}

/// True when every grant in `child` is subsumed by some grant in `parent`.
pub fn encloses(parent: &[String], child: &[String]) -> bool {
    child.iter().all(|c| parent.iter().any(|p| covers(p, c)))
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
        crate::api::FACET_FACET => format!("{META}:facets:{}", meta_action(action)),
        crate::api::SYSTEM_FACET => format!("{META}:system:read"),
        // Reading a pairing session is a dashboard's job; changing one is
        // an approver's. Collapsing both onto `read` would make a
        // read-only pairing view able to rewrite the session it displays.
        crate::api::PAIR_FACET => match action {
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
    matches!(facet, crate::api::SYSTEM_FACET | crate::api::PAIR_FACET)
}

/// The facet meta-facet has two permissions, not four: reading a
/// registration, and changing one.
fn meta_action(action: &str) -> &'static str {
    match action {
        "read" => "read",
        _ => "write",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersected_authority_requires_both_grants() {
        let patterns = ["*", "tasks:*", "*:read", "tasks:read", "lists:create", "meta:*", "meta:pairing:*", "meta:*:read"];
        let permissions = ["tasks:read", "tasks:create", "tasks:delete", "lists:read", "lists:create", "meta:pairing:read", "meta:pairing:approve", "meta:clients:read", "meta:clients:revoke"];
        for a in patterns {
            for b in patterns {
                let both = intersection(&[a.into()], &[b.into()]);
                for required in permissions {
                    assert_eq!(granted(&both, required), covers(a, required) && covers(b, required), "{a} ∩ {b}: {required}");
                }
            }
        }
    }

    fn g(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    // ------------------------------------------------------------ matching

    #[test]
    fn a_literal_grant_covers_only_itself() {
        assert!(covers("tasks:read", "tasks:read"));
        assert!(!covers("tasks:read", "tasks:create"));
        assert!(!covers("tasks:read", "lists:read"));
        assert!(!covers("tasks:read", "tasks"));
        assert!(!covers("tasks:read", "tasks:read:extra"));
    }

    #[test]
    fn a_trailing_star_covers_the_rest() {
        assert!(covers("tasks:*", "tasks:read"));
        assert!(covers("tasks:*", "tasks:delete"));
        assert!(covers("meta:*", "meta:pairing:approve"));
        assert!(covers("meta:pairing:*", "meta:pairing:approve"));
        // ...but there has to be a rest.
        assert!(!covers("tasks:*", "tasks"));
        assert!(!covers("meta:*", "meta"));
    }

    #[test]
    fn a_bare_star_covers_everything() {
        for p in ["tasks:read", "meta:pairing:approve", "anything", "a:b:c:d:e"] {
            assert!(covers("*", p), "{p}");
        }
    }

    #[test]
    fn a_star_in_the_middle_matches_exactly_one_segment() {
        assert!(covers("*:read", "tasks:read"));
        assert!(covers("*:read", "lists:read"));
        assert!(!covers("*:read", "tasks:create"));
    }

    /// The property the grammar exists for: reading everything and
    /// administering everything are different grants, and read-all cannot
    /// reach into meta by accident.
    #[test]
    fn read_everything_does_not_reach_meta() {
        assert!(!covers("*:read", "meta:facets:read"));
        assert!(!covers("*:read", "meta:server:read"));
        assert!(!covers("*:read", "meta:pairing:approve"));
        assert!(covers("*:read", "tasks:read"));
    }

    #[test]
    fn granted_asks_the_whole_set() {
        let held = g(&["tasks:read", "tasks:create", "lists:*"]);
        assert!(granted(&held, "tasks:read"));
        assert!(granted(&held, "lists:delete"));
        assert!(!granted(&held, "tasks:delete"));
        assert!(!granted(&held, "meta:facets:read"));
        assert!(!granted(&[], "tasks:read"));
    }

    // ------------------------------------------------------------ enclosure

    #[test]
    fn enclosure_narrows() {
        assert!(encloses(&g(&["*"]), &g(&["tasks:read", "meta:facets:write"])));
        assert!(encloses(&g(&["tasks:*"]), &g(&["tasks:read"])));
        assert!(encloses(&g(&["tasks:*", "lists:read"]), &g(&["lists:read", "tasks:delete"])));
        assert!(!encloses(&g(&["tasks:read"]), &g(&["tasks:create"])));
        assert!(!encloses(&g(&["tasks:*"]), &g(&["lists:read"])));
        assert!(!encloses(&g(&["*:read"]), &g(&["meta:facets:read"])));
    }

    /// A child pattern must be subsumed, not merely matched. This is the
    /// trap: `tasks:read` must not be allowed to mint `tasks:*`.
    #[test]
    fn a_narrow_parent_cannot_mint_a_wildcard_child() {
        assert!(!encloses(&g(&["tasks:read"]), &g(&["tasks:*"])));
        assert!(!encloses(&g(&["tasks:read"]), &g(&["*"])));
        assert!(!encloses(&g(&["tasks:read"]), &g(&["*:read"])));
        assert!(!encloses(&g(&["meta:pairing:read"]), &g(&["meta:*"])));
        assert!(!encloses(&g(&["meta:pairing:read"]), &g(&["meta:pairing:*"])));
    }

    /// Deliberately conservative: holding every action today is not the
    /// same as holding the wildcard, because a fifth action would change
    /// what the wildcard means.
    #[test]
    fn holding_every_action_does_not_amount_to_the_wildcard() {
        let all_four = g(&["tasks:read", "tasks:create", "tasks:update", "tasks:delete"]);
        assert!(!encloses(&all_four, &g(&["tasks:*"])));
        assert!(encloses(&all_four, &g(&["tasks:delete"])));
    }

    #[test]
    fn an_empty_child_is_enclosed_by_anything() {
        assert!(encloses(&g(&["tasks:read"]), &[]));
    }

    // ------------------------------------------------------------ validation

    #[test]
    fn grants_are_checked_for_shape() {
        for ok in ["tasks:read", "*", "meta:pairing:*", "a.b-c_d:read", "imap:sync", "x9:read"] {
            assert!(check_grant(ok).is_ok(), "rejected {ok}");
        }
        for bad in ["", "Tasks:read", "tasks:READ", "tasks::read", "tasks:read ", "-x:read", ":read", "ta sks:read"] {
            assert!(check_grant(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// A facet named `meta` — or `*` — would be a way to mint a meta
    /// permission by registering something.
    #[test]
    fn facet_names_cannot_reach_into_meta() {
        for bad in ["meta", "facet", "system", "pair", "*", "meta:facets", "a:b", "MyFacet", ""] {
            assert!(check_facet_name(bad).is_err(), "accepted facet name {bad:?}");
        }
        for ok in ["tasks", "lists", "imap", "sensors.kitchen", "notes-v2"] {
            assert!(check_facet_name(ok).is_ok(), "rejected facet name {ok}");
        }
    }

    #[test]
    fn the_cores_own_facets_answer_only_to_meta() {
        assert_eq!(for_facet("facet", "read"), "meta:facets:read");
        assert_eq!(for_facet("facet", "create"), "meta:facets:write");
        assert_eq!(for_facet("facet", "delete"), "meta:facets:write");
        assert_eq!(for_facet("system", "read"), "meta:system:read");
        assert_eq!(for_facet("tasks", "create"), "tasks:create");
    }

    /// Reading pairing requests must not be a way to change them.
    #[test]
    fn reading_a_pairing_is_not_approving_one() {
        assert_eq!(for_facet("pair", "read"), "meta:pairing:read");
        assert_eq!(for_facet("pair", "update"), "meta:pairing:approve");
        assert_eq!(for_facet("pair", "delete"), "meta:pairing:approve");
        assert!(!covers("meta:pairing:read", &for_facet("pair", "update")));
        assert!(core_managed("pair") && core_managed("system") && !core_managed("tasks"));
    }

    /// The open namespace: a grant for a facet nobody has registered is
    /// legal, and starts working the moment the facet exists.
    #[test]
    fn a_grant_for_an_unknown_namespace_is_legal_and_inert() {
        assert!(check_grant("newthing:read").is_ok());
        let held = g(&["newthing:read"]);
        assert!(granted(&held, "newthing:read"));
        assert!(!granted(&held, "tasks:read"));
    }

    /// A permission the core has no meaning for still delegates and
    /// encloses correctly, so applications can define their own.
    #[test]
    fn permissions_the_core_does_not_understand_still_enclose() {
        assert!(check_grant("imap:sync").is_ok());
        assert!(encloses(&g(&["imap:*"]), &g(&["imap:sync"])));
        assert!(!encloses(&g(&["imap:sync"]), &g(&["imap:*"])));
        assert!(granted(&g(&["imap:sync"]), "imap:sync"));
    }
}
