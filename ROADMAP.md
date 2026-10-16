# Roadmap

Items intentionally deferred from v1 (`mcp-hub-policy` 0.1.0) and ideas
for future directions. Each entry includes the requirement section it
came from where applicable.

For per-version release notes see [`CHANGELOG.md`](CHANGELOG.md).

## v0.2.x (shipped — first MCP-client-compatible release)

End-to-end working with Claude Desktop, MCP Inspector, ChatGPT
Desktop, Postman, and any conformant Streamable-HTTP client. See the
changelog for the per-version breakdown; the headline fixes vs 0.1.0:

- **Single-phase dispatch** (`request_filter` `Flow::Break`s with the
  synthesized response). Bypasses Flex Gateway's upstream-cluster
  check so the hub no longer needs a healthy backend (0.2.1).
- **Spec-compliant Streamable HTTP**: `notifications/*` → 202,
  unsupported methods → 405 + `Allow: POST`, GET with SSE Accept →
  200 + `text/event-stream` empty stub stream (0.2.2 / 0.2.3).
- **Tool-name regex compliance**: prefix separator switched from `.`
  to `_` so names match `^[a-zA-Z0-9_-]{1,64}$` (Claude Desktop /
  OpenAI tool-calling) (0.2.3).
- **Capabilities cleanup**: removed `logging` from advertised
  capabilities — declaring it without implementing
  `logging/setLevel` tripped MCP Inspector (0.2.4).

## v1 (shipped, 0.1.0)

- Virtual `/mcp` endpoint with suffix-match path (REQ §6.1).
- Server registry with up to 20 MCP servers, per-server `enabled`,
  `priority`, `headers`, `timeoutMs` overrides (REQ §6.3).
- Per-server auth: `none`, `bearerToken`, `basicAuth`, `apiKeyHeader`,
  `customHeaders` (REQ §6.5).
- `tools/list` / `resources/list` / `prompts/list` aggregation with
  configurable collision strategy (`prefix`, `reject`, `firstWins`,
  `lastWins`) (REQ §6.7, §6.8).
- `tools/call` / `resources/read` / `prompts/get` routing — fast path
  via `<server>_<name>` prefix; slow path via re-aggregation when the
  collision strategy is not `prefix` (REQ §6.9).
- Local handshake (`initialize`) with merged capabilities + hub-owned
  `serverInfo` (REQ §5).
- Notification fan-out (REQ §5).
- MCP-compliant error codes for unknown method, server unavailable,
  parse error, etc. (REQ §6.13).
- `strictMode` 404 vs. pass-through behavior on non-MCP paths.

## P1 — near-term (next minor version)

### 1. Streamable HTTP pass-through (REQ §6.12)

The hub currently buffers each upstream response before forwarding it to
the client. For `tools/call` results that emit `text/event-stream` or
chunked responses, the hub should:

- detect `content-type: text/event-stream` on the upstream response,
- open a streaming response back to the client without waiting for the
  upstream to complete,
- forward chunks bidirectionally until upstream closes.

PDK 1.8 supports streaming response bodies via `ResponseBodyState`.
Implementing this requires switching the response phase from
`send_response` (one-shot) to a streaming variant.

### 2. Manifest cache (TTL already wired)

`manifestCacheTtlSeconds` is already a config knob (default 60s) but
the response path re-aggregates on every routed call when the strategy
is not `prefix`. Implement an in-memory cache keyed by
`(method, collisionStrategy, generation)` so the slow-path resolver
only re-aggregates after the TTL elapses.

### 3. Per-route trust modes (parity with `acp-to-a2a`)

Mirror the `routeTrustModes` config from `acp-to-a2a` so individual
methods (`tools/call`, `resources/read`, ...) can be guarded with
distinct auth requirements. Useful when, e.g., `tools/list` is
anonymous-readable but `tools/call` requires a Capability Token.

### 4. Per-server health / readiness

Today, a failing upstream during `tools/list` is silently skipped. Add:

- a per-replica health bitmap updated on every call,
- a circuit breaker (open / half-open / closed) with configurable
  `failureThreshold` and `recoverySeconds`,
- emit `mcp-hub: server.<name>.healthy = false` log lines on state
  transition,
- expose the bitmap in the `GET /mcp` hub-info payload.

### 5. Observability (REQ §7.4, §7.5)

Add structured logs and Prometheus-shaped metrics emitted via the PDK
metrics API:

- `mcp_hub_request_count{method=...}` total requests served.
- `mcp_hub_upstream_latency_ms{server=...,method=...}` histogram.
- `mcp_hub_upstream_errors_total{server=...,reason=...}` counter.
- `mcp_hub_aggregation_partial_total{method=...}` counter.

Tracing (OpenTelemetry, REQ §12) is its own item.

## P2 — medium-term

### 6. Capability Tokens (parity with `acp-to-a2a`)

Verify inbound capability tokens against a JWKS, optionally re-mint
them for outbound calls when the upstream MCP server expects a
gateway-minted credential. Requires:

- `capTokenMode` (`disabled` / `verify_only` / `verify_and_remint` /
  `per_route`),
- `capTokenJwksUrl`, `capTokenIssuer`, `capTokenAudience`, leeway,
- per-route audit (which routes require ops, which scopes).

### 7. Tool-level authorization (REQ §12)

Once Capability Tokens are in, gate individual tool / resource /
prompt invocations on the token's `ops` claim. The hub already knows
which server owns each name; the `ops` mapping can be configured per
server or per `<server>.<tool>` pair.

### 8. OAuth2 client credentials per upstream (REQ §12)

For MCP servers that require an OAuth2 access token (rather than a
static bearer), let the hub mint and cache a `client_credentials`
token per upstream. Use the same token cache shape from
`acp-to-a2a`'s `cache.rs`.

### 9. mTLS to upstreams (REQ §7.3)

Surface `mtlsClientCertSecret` per server so individual upstreams can
require mutual TLS. The PDK 1.8 `Service` definition already supports
mTLS; the policy needs to wire it through the config schema.

### 10. Multi-tenant isolation (REQ §12)

Today every inbound client sees the union of every configured upstream.
Add an inbound tenant header (configurable name) and a tenant→server
allowlist so different tenants share the same gateway instance but see
different subsets of MCP servers.

## P3 — exploratory

### 11. Dynamic MCP discovery (REQ §12)

Instead of a static `mcpServers` array, accept a discovery URL that
returns a list of MCP servers. The hub polls the discovery endpoint
every `discoveryRefreshSeconds` and reconciles its server table.

### 12. Session affinity (REQ §12)

Stateful MCP servers can require that subsequent calls land on the
same upstream replica. Add a sticky-session cookie issued on the first
`initialize` and respected on subsequent calls.

### 13. Tool composition / chaining

A first-class hub feature where one tool's output is piped into another
tool's input via a declarative recipe. Borders on AI-orchestration
territory (REQ §2 declares this a non-goal for v1) but is a natural
extension once aggregation is solid.

### 14. WASM-side caching of `tools/list`

The PDK shared cache lets workers share state across requests. Cache
the aggregated `tools/list` payload with a generation number; bump the
generation when any upstream's `notifications/tools/list_changed`
arrives, or when the TTL expires.

### 15. SSE → WebSocket transcoding

Some MCP clients prefer WebSockets. The hub could front a WS endpoint
and translate to/from each upstream's Streamable HTTP transport.

## Operational improvements

- `make publish-asset-version` target that bumps `Cargo.toml` and the
  `definition/gcl.yaml` version label in lockstep.
- Helm chart / Terraform module for declarative deployment of the
  policy across an Anypoint org.
- Synthetic check shipped alongside the policy:
  `mcp-hub doctor --endpoint http://gw/mcp` runs `initialize` →
  `tools/list` → `tools/call` against a configured smoke tool.

## Known limitations

- Streaming responses are buffered (P1 #1).
- The runtime upstream table is fixed at policy load — adding /
  removing an upstream requires a re-publish (P3 #11).
- Secret fields (`token`, `password`, `apiKey`, `headers[].value`) are
  `security:sensitive` (masked in API Manager), but hold the literal
  value — the policy does not resolve `${secure::...}` placeholders.
  Integration with a secrets manager is not implemented.
