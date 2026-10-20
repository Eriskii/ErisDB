//! Exercise user-visible argument handling through the installed CLI entrypoint.

async fn erisdb(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_erisdb"))
        .env_clear()
        .args(args)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn endpoint_id_follows_the_seed_and_honors_its_override() {
    async fn endpoint_id(args: &[&str]) -> iroh::EndpointId {
        let output = erisdb(&[&["endpoint-id"], args].concat()).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    let default = endpoint_id(&["--secret", "cli-token-secret"]).await;
    assert_eq!(
        default,
        endpoint_id(&["--secret", "cli-token-secret"]).await
    );
    assert_eq!(default, erisdb::net::endpoint_id(b"cli-token-secret"));
    assert_ne!(
        default,
        endpoint_id(&["--secret", "another-token-secret"]).await
    );
    let overridden = endpoint_id(&[
        "--secret",
        "cli-token-secret",
        "--iroh-secret",
        "cli-endpoint-secret",
    ])
    .await;
    assert_ne!(default, overridden);
    assert_eq!(
        overridden,
        endpoint_id(&[
            "--secret",
            "rotated-token-secret",
            "--iroh-secret",
            "cli-endpoint-secret"
        ])
        .await
    );
}

#[tokio::test]
async fn serve_names_the_networks_it_knows_before_touching_a_store() {
    let output = erisdb(&[
        "serve",
        "--database-url",
        "postgres://nobody@127.0.0.1:1/erisdb",
        "--secret",
        "s",
        "--network",
        "carrier-pigeon",
    ])
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("internet, loopback, or a relay URL"),
        "{stderr}"
    );
}
