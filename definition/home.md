# MCP Hub Policy

Aggregates multiple upstream **MCP** (Model Context Protocol) servers
behind a single virtual `/mcp` endpoint. Built on PDK 1.8
(Rust → WASM) for MuleSoft Omni Gateway / Flex Gateway.

After the policy is applied, MCP clients (LLM agents, IDE extensions,
desktop apps) connect to one MCP endpoint that fans out across every
configured upstream. Tool, resource, and prompt manifests are merged on
the wire; per-server authentication, custom headers, and timeouts are
applied transparently.

## Methods served

The policy speaks the **MCP Streamable HTTP** transport on the
configured `mcpEndpoint` (default `/mcp`):

| JSON-RPC method      | Behaviour                                                                |
|----------------------|--------------------------------------------------------------------------|
| `initialize`         | Local handshake — hub identifies itself; capabilities are the union     |
| `ping`               | Local 200 (liveness probe)                                              |
| `tools/list`         | Aggregate from every enabled upstream; apply `collisionStrategy`         |
| `tools/call`         | Route to the upstream that owns the named tool                          |
| `resources/list`     | Aggregate                                                               |
| `resources/read`     | Route                                                                   |
| `prompts/list`       | Aggregate                                                               |
| `prompts/get`        | Route                                                                   |
| `notifications/*`    | Best-effort fan-out to every enabled upstream                           |
| _(unknown method)_   | `METHOD_NOT_FOUND` error                                                |

`GET /mcp` returns a hub-info JSON document — useful for clients that
probe the endpoint before opening a JSON-RPC session.

## Quick start

Minimal config — front two MCP servers, prefix any colliding tool
names with the server name:

```yaml
config:
  mcpEndpoint:        /mcp
  strictMode:         true
  collisionStrategy:  prefix          # prefix | reject | firstWins | lastWins

  mcpServers:
    - name:           jira
      endpoint:       https://jira.example.com/mcp
      enabled:        true
      authType:       bearerToken
      token:          ${secure::jira.token}

    - name:           slack
      endpoint:       https://slack.example.com/mcp
      enabled:        true
      authType:       apiKeyHeader
      headerName:     x-api-key
      apiKey:         ${secure::slack.key}
```

Required fields: a non-empty `mcpServers` array; each entry needs
`name` and `endpoint`. Everything else has sensible defaults.

## Per-server authentication

The hub picks the auth scheme per upstream from the configured
`authType`:

| `authType`       | Required fields                  | Header(s) emitted                         |
|------------------|----------------------------------|-------------------------------------------|
| `none`           | (none)                           | (no auth header)                          |
| `bearerToken`    | `token`                          | `Authorization: Bearer <token>`           |
| `basicAuth`      | `username`, `password`           | `Authorization: Basic base64(u:p)`        |
| `apiKeyHeader`   | `headerName`, `apiKey`           | `<headerName>: <apiKey>`                  |
| `customHeaders`  | (use `headers` array)            | every entry from the `headers` array      |

The `headers` array is always honored on top of the auth scheme — use
it for tenant headers, environment markers, or any non-auth metadata
the upstream expects (`x-env: prod`, `x-tenant: acme`, etc.).

## Collision strategies

Every `*list` method merges manifests from every enabled upstream. When
two upstreams advertise the same name, the configured `collisionStrategy`
decides what to do:

| Strategy     | Behaviour                                                                 |
|--------------|---------------------------------------------------------------------------|
| `prefix`     | Rename to `<serverName>.<originalName>` (default). Both entries kept.     |
| `reject`     | Fail aggregation with an `INTERNAL_ERROR` JSON-RPC error.                 |
| `firstWins`  | Keep the first upstream's entry, drop subsequent dupes.                   |
| `lastWins`   | Overwrite with the most recently seen entry.                              |

With `prefix`, `tools/call` accepts the `<server>.<tool>` form on the
inbound side and forwards the original (un-prefixed) name to the
upstream so the upstream sees its native namespace.

## Behavior in practice

- **Slow / failing upstreams** during aggregation are logged and
  skipped — partial degradation is preferred over total failure.
  `aggregationTimeoutMs` (default 5s) caps each upstream's contribution.
- **Per-call timeouts** during routed `tools/call` etc. default to
  `perRequestTimeoutMs` (30s) and may be overridden per server with
  `timeoutMs`.
- **Streaming** responses (`text/event-stream`, chunked) are accepted
  by the hub on the outbound `Accept`, but in v0 the hub buffers the
  full response before forwarding to the inbound client. Bidirectional
  streaming pass-through is on the roadmap.
- **Notifications** (JSON-RPC requests without `id`) fan out to every
  enabled upstream with errors swallowed; the hub responds 204.
- **`mcpServers` ordering** is `priority` first (lower = earlier),
  then config order. With `firstWins` / `lastWins` this controls which
  server "wins" a name collision.

## Endpoint matching

`mcpEndpoint` is treated as a suffix match — if API Manager mounts the
governed API at `/api/v1`, the policy still serves the hub at
`/api/v1/mcp` without re-configuration. Set `strictMode: true` (default)
to 404 anything that does not hit the MCP endpoint; set
`strictMode: false` to fall through to the gateway's own upstream so
the policy can stack with other APIs on the same instance.
