# Changelog

All notable changes to `mcp-hub-policy`. The 0.2.x line is the first
shippable, MCP-client-compatible release; 0.1.0 was an internal
preview.

## [0.3.5] — 2026-10-26

### Added

- `maxUpstreamResponseBytes` (default 8 MiB, 1–64 MiB): a larger
  upstream answer fails with `-32603` naming the server instead of being
  parsed and relayed.
- `requireCallerAuth` (default false): an MCP POST without
  `Authorization` gets HTTP 401 with `WWW-Authenticate` (pointing at the
  protected-resource metadata when configured), so MCP clients start
  their OAuth flow on connect. Only presence is checked.
- `resources/templates/list` answers `{"resourceTemplates": []}`
  (was `-32601`).
- The protected-resource metadata is also served at the RFC 9728 path
  form `/.well-known/oauth-protected-resource/<path>`.

### Changed

- `forwardHeaders` are sent on routed calls (`tools/call`,
  `resources/read`, `prompts/get`) only, not on the session handshake,
  `*/list` or notifications: those are shared by every caller of a
  static-auth server and cached without regard to the headers.
- `GET` on the MCP endpoint always answers 405 with `Allow: POST`
  (SSE clients got an empty stream that ended at once, which makes
  clients reconnect in a loop).
- One timeout budget (`aggregationTimeoutMs` / `perRequestTimeoutMs` /
  `timeoutMs`) covers the whole upstream call — handshake, call and the
  retry after an expired session — instead of each HTTP request. A
  session the call just opened is kept when the call itself times out,
  and running out of time on the retry handshake starts no backoff.
- Static-auth and per-caller (`passthrough`) sessions and manifests use
  separate caches, so callers sending many distinct tokens can no longer
  evict the entries every caller shares. Static-auth `*/list` answers up
  to 512 KB are cached (was 64 KB; per-caller answers stay at 64 KB).
- An upstream answering `-32601` to a `*/list` method counts as an empty
  list: it is cached and no longer logged or reported as skipped.
- Server names may contain only letters, digits, `_` and `-`; other
  names take the server out of service. Prefixed tool or prompt names
  over 64 characters are logged as a warning.
- JSON-RPC: batches, `"id": null` (or a non-string, non-number id) and a
  missing `method` answer `-32600`; a JSON-RPC response from the client
  gets 202.
- A caller without a passthrough credential is logged at debug level
  (was warn) when a `*/list` skips the server.

### Fixed

- An SSE upstream's `content-type` is matched case-insensitively.

## [0.3.4] — 2026-10-26

### Added

- `forwardHeaders` (default `traceparent`, `tracestate`,
  `x-correlation-id`): caller headers copied to every upstream.
  Credential, transport and server-auth headers can't be listed.
- `upstreamFailureBackoffSeconds` (default 10, `0` = off): after an
  upstream handshake fails, the upstream isn't contacted again for that
  long (per server, per caller for `passthrough`); routed calls get
  HTTP 503 with `Retry-After`. Only failures a caller can't cause count
  (unreachable / timeout, 408, 429, 5xx, JSON-RPC error, invalid
  answer); a shorter upstream `Retry-After` wins; a timeout only backs
  off calls with the same or a shorter timeout.
- `exposeUpstreamErrors` (default false): adds the first 1 KB of an
  upstream HTTP error body to `error.data.body` and lists servers a
  `*/list` left out in `result._meta["mcp-hub/skipped"]`.
- Routed-call errors carry `error.data.reason` (`missing_credentials`,
  `unauthorized`, `unavailable`, `error`, `misconfigured`).

### Changed

- Upstream 429 and 5xx keep their HTTP status and `Retry-After`
  (previously HTTP 200). Other upstream HTTP errors still answer
  `-32010` on HTTP 200, with `error.data.httpStatus`.
- The hub requests MCP `2025-06-18` from upstreams, adopts the version
  each upstream answers with and sends it as `MCP-Protocol-Version`.
- An invalid `mcpServers` entry no longer rejects the whole policy: it
  is logged and taken out of service (`*/list` skip it, its calls get
  `-32010` with the reason). Invalid hub-wide settings fail closed —
  every MCP request gets HTTP 503 with the reason — instead of failing
  policy start-up. Duplicate server names isolate every copy.
- A `passthrough` server whose `headers` override every passthrough
  header is rejected as misconfigured (use `customHeaders`).
- A `jsonrpc` other than `"2.0"` answers `-32600` (was `-32700`).
- A server's `headers` entry replaces any same-named header the hub
  would send (it was sent in addition, e.g. two `Authorization`
  values); transport headers in `headers` make the server invalid.
- `forwardHeaders` can't list headers of `customHeaders` servers or of
  invalid entries.
- An upstream that fails at the transport level mid-session is
  re-initialized on the next call (so the backoff applies).
- More hop-by-hop headers (`keep-alive`, `upgrade`, `te`, `trailer`,
  `expect`, `proxy-connection`, pseudo-headers) are reserved.

### Fixed

- A request method sent without an `id` (e.g. `tools/call`) is no longer
  forwarded to upstreams as a notification; only `notifications/*` are.
- Outbound JSON-RPC ids include a random per-worker part, so workers
  sharing an upstream session cannot send duplicate ids.
- Passthrough servers on headers other than `Authorization` (e.g.
  `client_id, client_secret`) no longer answer 401s with the hub's OAuth
  `Bearer resource_metadata=…` challenge. The upstream's own challenge
  (e.g. `Client-ID-Enforcement`) is relayed; a call missing a header gets
  HTTP 401 without a challenge, and `error.data.missingHeaders` names
  what is missing. The error message now lists the missing headers.
- An empty passthrough header value counts as missing.
- `prefix` routing: when several servers' prefixes match a name (server
  `a` + tool `b_c` vs server `a_b` + tool `c`), the call goes to the
  entry `tools/list` showed instead of the longest server prefix, which
  could run a different tool. Disabled servers no longer take part in
  prefix matching.
- The upstream handshake (`initialize`) uses the calling request's
  timeout, so a dead upstream can't hold `*/list` past
  `aggregationTimeoutMs` (it waited up to `perRequestTimeoutMs`).

## [0.3.3] — 2026-10-23

### Fixed

- `authType: passthrough` can forward several caller headers:
  `headerName` takes a comma-separated list (e.g.
  `client_id, client_secret` for an upstream behind Client ID
  Enforcement). All listed headers must be present; otherwise list
  calls skip the server and routed calls return HTTP 401. Previously
  only one header was forwarded, so such upstreams always answered 401.

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
