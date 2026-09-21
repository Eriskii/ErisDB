# One-shot plugins

A plugin is a pure request/response executable. It is not a daemon, does
not listen on a port, and does not register over the network. One
`POST /v1/call` starts one new process; the core writes one JSON invocation
to its stdin, reads one response header from stdout, streams the remaining
stdout to the client, and waits for the process to exit.

Plugin calls do **not** read or write Postgres. There is no job row, queue,
conversation, retry, callback, or plugin-owned state in the core. If the
core or connection dies, that invocation dies. The caller decides whether
to make another request.

The core holds only deployment-derived configuration while it runs: the
manifests loaded at startup and a semaphore bounding child processes. A
restart reconstructs both. Every core replica therefore needs the same
manifest directory, executables, and environment variables.

## Manifest

`erisdb serve --plugin-dir /etc/erisdb/plugins.d` loads every `*.json` file
in that directory. A manifest declares the executable, the environment it
may receive, and one or more operations:

```json
{
  "protocol": 1,
  "name": "openai",
  "description": "OpenAI API calls through a fresh process per request",
  "executable": "/usr/local/libexec/erisdb/erisdb-plugin-openai",
  "environment": {
    "OPENAI_API_KEY": true,
    "OPENAI_BASE_URL": false
  },
  "timeout_secs": 600,
  "operations": {
    "chat.completions": {
      "description": "Create or stream an OpenAI Chat Completion",
      "permission": "openai:chat",
      "request_schema": {
        "type": "object",
        "required": ["model", "messages"],
        "properties": {
          "model": {"type": "string", "minLength": 1},
          "messages": {"type": "array", "minItems": 1}
        },
        "additionalProperties": true
      }
    }
  }
}
```

Names and operation names use the same lowercase segment grammar as
permissions. An operation's concrete permission must live under its
plugin's namespace and cannot contain a wildcard. The request schema is
compiled with the same JSON Schema implementation used for facets;
external `$ref` values are refused.

The environment map is an allowlist over an otherwise empty child
environment. `true` means the variable is required at invocation time;
`false` means it is copied only when present. Values never belong in the
manifest. A plugin receives no capability token and no undeclared core
secret.

Manifests are machine configuration, not database content. Installing or
changing one is an operator action followed by a core restart. This is
intentional: letting a token write an executable path would turn a data
permission into remote code execution.

## Calling and discovery

A call uses one fixed envelope:

```http
POST /v1/call
Authorization: Bearer …
Content-Type: application/json
```

```json
{
  "plugin": "openai",
  "operation": "chat.completions",
  "input": {
    "model": "gpt-5.4",
    "messages": [{"role": "user", "content": "hello"}],
    "stream": true
  }
}
```

The core finds the manifest, requires the operation's permission, validates
`input`, and only then starts the process. `GET /v1/plugins` returns the
descriptions and request schemas visible to the caller; operations its token
cannot invoke are omitted.

The output status, content type, allowed response headers, and body all come
from the executable. A JSON plugin can return JSON. A streaming plugin can
return `text/event-stream`; bytes are forwarded as they arrive rather than
buffered until exit.

## Process protocol v1

Stdin is one JSON line followed by EOF:

```json
{
  "protocol": 1,
  "plugin": "openai",
  "operation": "chat.completions",
  "input": {"model": "…", "messages": []},
  "context": {"user": "alice"}
}
```

`context.user` is present only when the caller's signed capability carries
one. The process does not receive the capability or its grants; the core has
already enforced the operation permission.

Stdout begins with one JSON line:

```json
{
  "protocol": 1,
  "status": 200,
  "content_type": "text/event-stream",
  "headers": {"x-request-id": "req_…"}
}
```

Every following byte is the response body. The process should flush after
streaming chunks. Stderr is for operator diagnostics and is bounded before
being logged. Hop-by-hop, credential, cookie, content-length and
content-type headers in `headers` are discarded; `content_type` is the one
authoritative spelling. The core adds `Cache-Control: no-store`.

If the client drops the response, the core kills the process. It also kills
it when `timeout_secs` passes, including when stdout has closed but the process
has not exited. A nonzero exit fails the response stream. The core reaps the
process before making its slot available again. At most 32 plugin processes
run per core replica, a call body is limited to 16 MiB, and the response-header
line is limited to 16 KiB while being read.

Installed plugin code is trusted deployment code. A separate process gives
crash and lifetime isolation, not a hostile-code sandbox: it runs as the
core's service user under the service unit's filesystem, syscall and network
restrictions.

## OpenAI Chat Completions

The shipped `erisdb-plugin-openai` forwards `input` as the request body of
`POST /v1/chat/completions`. It does not reinterpret messages, tools,
`tool_choice`, structured output settings, model-specific options, or future
fields. OpenAI's JSON response or SSE stream is returned unchanged.
The plugin uses the
[OpenAI Chat Completions reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create).

Function tools behave exactly as they do through the OpenAI API: the model's
tool-call object is returned to the client. The client executes its tool and
makes its next Chat Completions request with the resulting messages. ErisDB
does not invent or execute a second tool protocol.

Install it:

```sh
cargo build --release --manifest-path erisdb/Cargo.toml
sudo install -d -m 0755 /usr/local/libexec/erisdb /etc/erisdb/plugins.d
sudo install -m 0755 erisdb/target/release/erisdb-plugin-openai \
  /usr/local/libexec/erisdb/
sudo install -m 0644 deploy/plugins/openai.json /etc/erisdb/plugins.d/
```

Set `ERISDB_PLUGIN_DIR=/etc/erisdb/plugins.d` and `OPENAI_API_KEY` in the
root-owned mode-0600 `/etc/erisdb/erisdb.env`, then restart the core. Optional
`OPENAI_BASE_URL`, `OPENAI_ORGANIZATION`, and `OPENAI_PROJECT` values are
passed when present. A client pairs for `openai:chat`.

The OpenAI credential stays in the service environment, is copied only into
the fresh OpenAI process, and is never sent to a client or stored in
Postgres.
