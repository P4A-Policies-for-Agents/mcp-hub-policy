# Changelog

All notable changes to `mcp-hub-policy`. The 0.2.x line is the first
shippable, MCP-client-compatible release; 0.1.0 was an internal
preview.

## [0.3.2] — 2026-10-16

### Added

- `manifestCacheTtlSeconds` (default 60) now takes effect: each
  upstream's `*/list` answer is cached per server — and per caller for
  `passthrough` servers — and reused until it expires. Failed answers
  and answers over 64 KB are not cached; a configuration change starts
  a fresh cache. `0` disables it.
- Upstream session keys are scoped by a hash of the policy
  configuration, so a configuration change starts fresh sessions.

### Changed

- `*/list` aggregation and notification fan-out call all upstreams
  concurrently instead of one after another.
- `diagnosticContinueFlow` is deprecated and ignored (it was never
  implemented); a warning is logged when it is set.

### Fixed

- `resources/read` is routed to the server that listed the resource's
  `uri` (previously always `RESOURCE_NOT_FOUND`), whatever the collision
  strategy does with duplicate resource names.
- SSE upstream responses: notifications sent before the response are
  skipped (previously the first event was taken, giving `result: null`).
  A stream with only notifications is now an error (`-32603` on routed
  calls; the server is skipped during `*/list`) instead of an empty
  success.

## [0.3.1] — 2026-10-15

### Changed

- `token`, `password`, `apiKey` and `headers[].value` are marked
  `security:sensitive` in the schema (replaces the undocumented
  `format: password`), so API Manager masks them.
- Maximum number of `mcpServers` raised from 20 to 50 (`MAX_SERVERS`).

### Fixed

- Documentation examples used `${secure::...}` placeholders, which the
  policy never resolved; secrets are entered as literal values.

## [0.3.0] — 2026-10-14

### Added

- `authType: passthrough` — forwards the caller's `Authorization` (or
  `headerName`) to that upstream only. A same-named `headers` entry
  overrides the caller's value.
- Upstream 401/403 (or a passthrough upstream called without a token)
  on a routed call is returned as HTTP 401/403 with `WWW-Authenticate`.
- RFC 9728 protected-resource metadata at
  `<resourceUrl>/.well-known/oauth-protected-resource`
  (`resourceUrl`, `authorizationServers`, `scopesSupported`).
- `allowedOrigins` — optional `Origin` allowlist (HTTP 403 otherwise).
- `upstreamSessionTtlSeconds` (default 1800).

### Changed

- The hub is stateless toward clients: `initialize` no longer issues an
  `mcp-session-id` and inbound ids are ignored (previously minted but
  never validated, and a client-chosen id was echoed back).
- `initialize` negotiates `protocolVersion` instead of echoing it; an
  unsupported `MCP-Protocol-Version` header is rejected with HTTP 400.
- Upstream MCP sessions live in the PDK shared cache, keyed per server
  and — for passthrough upstreams — per caller (SHA-256 of the token),
  so users never share an upstream session. An upstream 404 triggers
  one re-initialize and retry.
- The hub sends `notifications/initialized` to each upstream after its
  `initialize`; outbound notifications omit `id`. The client's own
  `notifications/initialized` is no longer fanned out.
- `*/list` aggregation skips upstreams that fail or reject the caller
  (unchanged degradation behaviour, now shared with owner lookup).
- Outbound JSON-RPC ids are unique per call (previously constant per
  method/server, so concurrent callers reused ids within a shared
  upstream session).
- Client notifications are forwarded only to the caller's own
  passthrough sessions; `notifications/cancelled` is never forwarded.
- An upstream that rejects `initialize` is no longer cached as
  initialized.
- A static-auth upstream answering 401/403 stays a JSON-RPC error
  (`-32010`) — the client can't fix the hub's credential.
- Servers are paired with their endpoints by name instead of by sorted
  position.
- `passthrough` rejects transport/session header names
  (`mcp-session-id`, `host`, `content-type`, …).

## [0.2.5] — 2026-05-22

### Changed

- Exchange asset category changed from `AI` to `MCP` so the policy
  surfaces in the MCP category in Anypoint Exchange / API Manager
  (no behavioural change).

## [0.2.4] — 2026-05-22

### Fixed

- Removed the `logging` capability from `initialize`'s `capabilities`
  block. Strict MCP clients (MCP Inspector) errored with *"Server
  declares logging capability but doesn't implement method:
  'logging/setLevel'"*. The hub does not expose client-controllable
  logging, so the capability must not be advertised. A regression test
  guards against re-introducing this and any other unsupported
  capability (`completions`, `experimental`).

## [0.2.3] — 2026-05-22

### Fixed

- **Tool-name regex compliance.** Aggregated tool names now use `_` as
  the prefix separator (`erp_get_inventory`) instead of `.`
  (`erp.get_inventory`). Claude Desktop and OpenAI tool-calling
  validate names against `^[a-zA-Z0-9_-]{1,64}$`, which forbids `.`.
  The fast-path resolver in `resolve_owner_fast` was rewritten to
  longest-prefix-match against configured server names, so server
  names containing `_` still resolve correctly.
- **MCP Inspector SSE fallback.** `GET /mcp` now answers
  200 + `text/event-stream` when the request advertises
  `Accept: text/event-stream` (Inspector's legacy SSE fallback,
  EventSource probes). Plain GETs still get 405 + `Allow: POST`.

## [0.2.2] — 2026-05-22

### Fixed

- **`GET /mcp` no longer returns 200/JSON.** Streamable-HTTP clients
  open a GET expecting a server-initiated SSE stream; returning
  `application/json` triggered *"Invalid content type, expected
  text/event-stream"*. Now answers 405 + `Allow: POST` (later
  enhanced in 0.2.3 to fall back to an empty SSE for SSE Accepts).
- **Notifications return 202, not 204.** undici (the fetch impl
  underneath MCP Inspector) rejects 204 in its `Response` constructor
  with *"Invalid response status code 204"*. Per MCP Streamable HTTP
  spec, notifications get 202.

## [0.2.1] — 2026-05-22

### Fixed

- **`POST /mcp` no longer returns `503 no healthy upstream`.**
  Collapsed the request/response pipeline into a single `request_filter`
  that `Flow::Break`s with the synthesized response. Previously the
  policy waited until the response phase to call `send_response`, but
  Flex Gateway's upstream-cluster check fired first when the API
  instance had no healthy backend (the normal case for a hub that
  synthesizes every response itself).

## [0.2.0] — 2026-05-21

### Added

- Per-upstream `mcp-session-id` cache + lazy `initialize` so
  Streamable-HTTP MCP servers (which gate post-`initialize` traffic
  on a session id) work end-to-end.
- SSE response parser (`extract_jsonrpc_from_sse`) so the hub can
  consume upstreams that respond with `text/event-stream` containing a
  single JSON-RPC payload.
- `notifications/initialized` is sent to each upstream after the hub's
  internal `initialize`.

## [0.1.0] — internal preview

- Virtual `/mcp` endpoint with suffix-match path.
- Server registry with up to 20 upstream MCP servers, per-server
  `enabled`, `priority`, `headers`, `timeoutMs` overrides.
- Per-server auth: `none`, `bearerToken`, `basicAuth`, `apiKeyHeader`,
  `customHeaders`.
- `tools/list` / `resources/list` / `prompts/list` aggregation with
  configurable collision strategy (`prefix`, `reject`, `firstWins`,
  `lastWins`).
- `tools/call` / `resources/read` / `prompts/get` routing.
- `strictMode` 404 vs. pass-through behaviour on non-MCP paths.
- 32 unit + integration tests.
