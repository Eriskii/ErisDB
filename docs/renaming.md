# Moving to ErisDB

The project and repository are now **ErisDB**. The core, Rust client, MCP
bridge, web apps, Android apps, and deployment files live together here.

The Rust packages are `erisdb`, `erisdb-client`, and `erisdb-mcp`. The
commands are `erisdb`, `erisdb-mcp`, and `erisdb-plugin-openai`. Build each
package using its own `Cargo.toml`; the Android apps use `erisdb-client`.

## Existing deployments

Environment variables now use `ERISDB_` in place of `BEZEL_`, including
the core secret, optional Iroh secret, plugin directory, MCP settings, and
poker settings. Copy the existing values into the renamed variables.
Keep the same `DATABASE_URL` and secret values to keep the same data,
tokens, and endpoint identity.

Install the renamed binaries and units from `deploy/` and `poker/`.
The templates use `/etc/erisdb`, the `erisdb` service account, and
`/usr/local/libexec/erisdb` for plugins. Adapt those paths and ownership to
the existing deployment, update plugin manifests, and stop the old service
and timer before starting `erisdb.service` and `erisdb-poker.timer`.
An existing Postgres database and role do not need to be renamed.

Rebuild the native client library as `liberisdb_client.so` before packaging
either Android app. Their source namespace is `dev.erisdb`, while the
application IDs remain `dev.bezel.lists` and `dev.bezel.tasks` so signed
updates retain access to existing app data.

## Compatibility

The following identifiers retain their original bytes because they name
existing protocols, identities, or stored data:

- Iroh ALPN `bezel/0` and key-derivation tag `bezel/iroh-endpoint-key/0`.
- Pairing URI prefix `bezel://pair/`, capability prefix `bz1`, and HTTP
  client header `X-Bezel-Client`.
- PostgreSQL notification channel `bezel_changes` and all existing SQL
  migration contents and checksums.
- Browser storage keys and Android preferences, keystore aliases, and
  application IDs.

These are compatibility identifiers; all new commands, configuration
templates, source packages, and project branding use ErisDB.
