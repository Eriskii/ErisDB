# Documentation

To build a client, start with [Client development](client-development.md) and
follow its links to the request and response examples in the [Server API](api.md).
The implementation examples below show how the included clients use those routes.

| File | Contents |
|---|---|
| [Server API](api.md) | HTTP/Iroh routes, request and response examples, authentication, errors, limits, and server CLI commands. |
| [Permissions](permissions.md) | Grant syntax and authorization rules. |
| [Capabilities](capabilities.md) | Token format, delegation, expiry, and refresh. |
| [Facets](facets.md) | Data schemas, validation, and the task and list definitions. |
| [Change feed](change-feed.md) | Polling, SSE, history, and revert. |
| [The mesh](mesh.md) | How ErisDB joins its owner's mesh, the passphrase, and how apps get in through a device. |
| [Installations](clients.md) | The app's key as its credential, permission changes, and revocation by the operator or the mesh. |
| [Client development](client-development.md) | Calling the server and keeping a local cache. |
| [Operations](operations.md) | Configuration, database initialization, backups, transport, and deployment. |
| [Plugins](plugins.md) | Implemented executable operations and their process protocol. |
| [Architecture](architecture.md) | Persistent concepts, transaction ownership, stream isolation, process supervision, and production CLOC measurement. |

Client-specific interfaces are documented with their implementations:
[MCP client](../clients/mcp/README.md),
[Rust and Android client library](../erisdb-client/README.md),
[Tasks Android app](../apps/tasks-android/README.md), and
[Lists Android app](../apps/lists-android/README.md).
