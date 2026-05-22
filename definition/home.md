# MCP Hub Policy

Aggregates multiple upstream **MCP** (Model Context Protocol) servers
behind a single virtual `/mcp` endpoint. Built on PDK 1.8
(Rust → WebAssembly) for MuleSoft Flex Gateway / Omni Gateway.

After the policy is applied, MCP clients (Claude Desktop, MCP Inspector,
ChatGPT Desktop, Postman, custom agents) connect to one MCP endpoint
that fans out across every configured upstream. Tool, resource, and
prompt manifests are merged on the wire; per-server authentication,
custom headers, and timeouts are applied transparently.

## Apply this policy to an MCP-typed API instance

Create the API instance in API Manager with **type = MCP**. The hub
itself is the MCP endpoint — the upstream URI on the instance is
**never used**, so any reachable URL satisfies API Manager's validation
(common choice: `https://anypoint.mulesoft.com`). Every recognized
request is short-circuited in the policy's request phase, so the
upstream is never contacted.

## Methods served

The policy speaks the **MCP Streamable HTTP** transport on the
configured `mcpEndpoint` (default `/mcp`):

| JSON-RPC method      | Behaviour                                                                |
|----------------------|--------------------------------------------------------------------------|
| `initialize`         | Local handshake — hub identifies itself and mints `mcp-session-id`       |
| `ping`               | Local 200 (liveness probe)                                               |
| `tools/list`         | Aggregate from every enabled upstream; apply `collisionStrategy`         |
| `tools/call`         | Route to the upstream that owns the named tool                           |
| `resources/list`     | Aggregate                                                                |
| `resources/read`     | Route                                                                    |
| `prompts/list`       | Aggregate                                                                |
| `prompts/get`        | Route                                                                    |
| `notifications/*`    | Best-effort fan-out → **202 Accepted** (no body)                         |
| _(unknown method)_   | `METHOD_NOT_FOUND` JSON-RPC error                                        |

`GET /mcp` serves the SSE-fallback path used by some clients (MCP
Inspector legacy SSE, EventSource probes): if the request advertises
`Accept: text/event-stream` the policy answers 200 with an immediate
empty SSE stream; otherwise it returns 405 + `Allow: POST`.

## Quick start

Minimal config — front three MCP servers, prefix any colliding tool
names with the server name:

```yaml
config:
  mcpEndpoint:        /mcp
  strictMode:         true
  collisionStrategy:  prefix          # prefix | reject | firstWins | lastWins

  mcpServers:
    - name:           erp
      endpoint:       https://erp-mcp.example.com/mcp
      enabled:        true
      authType:       bearerToken
      token:          ${secure::erp.token}

    - name:           crm
      endpoint:       https://crm-mcp.example.com/mcp
      enabled:        true
      authType:       apiKeyHeader
      headerName:     x-api-key
      apiKey:         ${secure::crm.key}

    - name:           kb
      endpoint:       https://kb-mcp.example.com/mcp
      enabled:        true
      authType:       none
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

The `headers` array is always honoured on top of the auth scheme — use
it for tenant headers, environment markers, or any non-auth metadata
the upstream expects (`x-env: prod`, `x-tenant: acme`, etc.).

## Collision strategies

Every `*list` method merges manifests from every enabled upstream. When
two upstreams advertise the same name, the configured `collisionStrategy`
decides what to do:

| Strategy     | Behaviour                                                                            |
|--------------|--------------------------------------------------------------------------------------|
| `prefix`     | Rename to `<serverName>_<originalName>` (default). Both entries kept.                |
| `reject`     | Fail aggregation with an `INTERNAL_ERROR` JSON-RPC error.                            |
| `firstWins`  | Keep the first upstream's entry, drop subsequent dupes.                              |
| `lastWins`   | Overwrite with the most recently seen entry.                                         |

The prefix uses `_` (not `.`) because MCP clients including Claude
Desktop and OpenAI's tool-calling validate names against
`^[a-zA-Z0-9_-]{1,64}$`. With `prefix`, `tools/call` accepts the
`<server>_<tool>` form on the inbound side and forwards the original
(un-prefixed) name to the upstream so the upstream sees its native
namespace.

## Behaviour in practice

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
  enabled upstream with errors swallowed; the hub responds **202**.
- **`mcpServers` ordering** is `priority` first (lower = earlier),
  then config order. With `firstWins` / `lastWins` this controls which
  server "wins" a name collision.
- **Capabilities** advertised on `initialize` are exactly `tools`,
  `resources`, `prompts`. Other capabilities (`logging`, `completions`,
  `experimental`) are *not* advertised because the hub does not
  implement their methods — advertising without implementing trips
  strict clients (e.g. MCP Inspector errors out with "Server declares
  X capability but doesn't implement method").

## Endpoint matching

`mcpEndpoint` is treated as a suffix match — if API Manager mounts the
governed API at `/api/v1`, the policy still serves the hub at
`/api/v1/mcp` without re-configuration. Set `strictMode: true` (default)
to 404 anything that does not hit the MCP endpoint; set
`strictMode: false` to fall through to the gateway's own upstream so
the policy can stack with other APIs on the same instance.
