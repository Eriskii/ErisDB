//! The bytes other parties hold on to. Tokens live in apps' and daemons'
//! storage, and the endpoint id is the key a device approved: neither may
//! change shape under a deployment.

const SECRET: &[u8] = b"golden-secret";

#[test]
fn a_capability_token_is_byte_for_byte_stable() {
    let cap = erisdb::auth::Capability {
        grants: vec!["tasks:read".into(), "meta:*".into()],
        exp: Some(4_102_444_800),
        user: Some("alice".into()),
        max_exp: Some(4_105_036_800),
        client: Some("0b8e2f4a-9c1d-4e6f-8a7b-3c2d1e0f9a8b".parse().unwrap()),
    };
    let token = erisdb::auth::mint_capability(SECRET, &cap).unwrap();
    assert_eq!(
        token,
        "erisdb1.eyJncmFudHMiOlsidGFza3M6cmVhZCIsIm1ldGE6KiJdLCJleHAiOjQxMDI0NDQ4MDAsInVzZXIiOiJhbGljZSIsIm1heF9leHAiOjQxMDUwMzY4MDAsImNsaWVudCI6IjBiOGUyZjRhLTljMWQtNGU2Zi04YTdiLTNjMmQxZTBmOWE4YiJ9.eneazUxxoGPiqekCJjDNrQRbn9UQ7UX7fi0Dc32Jxgg"
    );
    assert_eq!(erisdb::auth::verify(SECRET, &token).unwrap(), cap);
    assert!(erisdb::auth::verify(b"another-secret", &token).is_err());
}

#[test]
fn the_endpoint_id_follows_from_the_secret_alone() {
    assert_eq!(
        erisdb::net::endpoint_id(SECRET).to_string(),
        "e78f31dc2470f54f035576d7791452e196e0ff62118aa90f3604ad099198b808"
    );
}
