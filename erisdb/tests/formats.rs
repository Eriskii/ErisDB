//! The bytes other parties hold on to. Tokens live in apps' storage,
//! identities in Postgres rows, endpoint ids in paired devices, tickets in
//! printed QR codes, and fingerprints on two screens at once: none of them
//! may change shape under a deployment.

use erislogin::identity::Identity;
use erislogin::ticket::Ticket;

const SECRET: &[u8] = b"golden-secret";
const SESSION: &str = "5f0c7c1e-3f55-4a53-9d1e-2c1b7b8f9a10";
const EID: &str = "e718b50236b0b98637fbf39cb4040e79800094313dc195e221e8e075304a6a06";

fn session() -> uuid::Uuid {
    SESSION.parse().unwrap()
}

#[test]
fn a_capability_token_is_byte_for_byte_stable() {
    let cap = erisdb::auth::Capability {
        grants: vec!["tasks:read".into(), "meta:*".into()],
        exp: Some(4_102_444_800),
        user: Some("alice".into()),
        max_exp: Some(4_105_036_800),
        pair: Some(SESSION.into()),
        client: Some("0b8e2f4a-9c1d-4e6f-8a7b-3c2d1e0f9a8b".parse().unwrap()),
    };
    let token = erisdb::auth::mint_capability(SECRET, &cap).unwrap();
    assert_eq!(
        token,
        "erisdb1.eyJncmFudHMiOlsidGFza3M6cmVhZCIsIm1ldGE6KiJdLCJleHAiOjQxMDI0NDQ4MDAsInVzZXIiOiJhbGljZSIsIm1heF9leHAiOjQxMDUwMzY4MDAsInBhaXIiOiI1ZjBjN2MxZS0zZjU1LTRhNTMtOWQxZS0yYzFiN2I4ZjlhMTAiLCJjbGllbnQiOiIwYjhlMmY0YS05YzFkLTRlNmYtOGE3Yi0zYzJkMWUwZjlhOGIifQ.l_bFnPblepUkIyI3A8SP3k5hxk6YZhjKPonbTa6wynY"
    );
    assert_eq!(erisdb::auth::verify(SECRET, &token).unwrap(), cap);
    assert!(erisdb::auth::verify(b"another-secret", &token).is_err());
}

#[test]
fn an_installation_identity_is_stored_as_it_always_is() {
    let identity = Identity::Iroh(EID.into());
    let stored = serde_json::to_string(&identity).unwrap();
    assert_eq!(stored, format!(r#"{{"kind":"iroh","key":"{EID}"}}"#));
    assert_eq!(serde_json::from_str::<Identity>(&stored).unwrap(), identity);
}

#[test]
fn both_screens_show_the_same_fingerprint_as_ever() {
    let domain = erisdb::installation::PAIR_DOMAIN;
    let requested = ["tasks:read".to_string(), "tasks:create".to_string()];
    assert_eq!(Identity::Iroh(EID.into()).fingerprint(domain, &session(), &requested), "08FC-6C59-7080");
    let browser = Identity::Browser("tMNl5Xr1UF5oz0tLgctGskAAFN2mmd_MHTaUNW9LCww".into());
    assert_eq!(browser.fingerprint(domain, &session(), &["*".to_string()]), "F48C-9B69-48C8");
}

#[test]
fn the_endpoint_id_follows_from_the_secret_alone() {
    assert_eq!(
        erisdb::net::endpoint_id(SECRET).to_string(),
        "e78f31dc2470f54f035576d7791452e196e0ff62118aa90f3604ad099198b808"
    );
}

#[test]
fn a_ticket_encodes_and_parses_under_the_erisdb_scheme() {
    let ticket =
        Ticket::new("erisdb1.a.b".into(), Some(EID.into()), Some("https://db.example.com".into()), Some("my-laptop".into()))
            .unwrap();
    let encoded = ticket.encode(erisdb::APP).unwrap();
    assert_eq!(
        encoded,
        "erisdb://pair/eyJ2IjoxLCJuYW1lIjoibXktbGFwdG9wIiwiZWlkIjoiZTcxOGI1MDIzNmIwYjk4NjM3ZmJmMzljYjQwNDBlNzk4MDAwOTQzMTNkYzE5NWUyMjFlOGUwNzUzMDRhNmEwNiIsInVybCI6Imh0dHBzOi8vZGIuZXhhbXBsZS5jb20iLCJ0b2tlbiI6ImVyaXNkYjEuYS5iIn0"
    );
    assert_eq!(Ticket::parse(erisdb::APP, &encoded).unwrap(), ticket);
}
