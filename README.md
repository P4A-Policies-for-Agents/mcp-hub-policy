# mcp-hub-policy

A custom **MuleSoft Flex Gateway / Omni Gateway** policy that turns one
governed API into a **virtual MCP aggregation endpoint**. AI agents and
MCP clients connect to a single `/mcp` URL; the policy talks to every
configured upstream MCP server, merges their tool / resource / prompt
manifests, applies per-server auth, and routes invocations to the
owning upstream.

Built with [PDK 1.8](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview)
(Rust → WebAssembly).

---

## Purpose

Model Context Protocol (MCP) is the emerging standard that LLM-based
agents (Claude Desktop, Cursor, Cline, MCP Inspector, ChatGPT Desktop,
ServiceNow's agent runtime, etc.) use to discover and invoke tools.
Each enterprise MCP server exposes its own catalogue, its own auth, its
own URL. As soon as you have more than one — an ERP server, a CRM
server, a knowledge-base server — every client has to be reconfigured
with a separate connection per server, and every server has to be
exposed individually through your gateway.

`mcp-hub-policy` removes both problems. It runs inside Anypoint Flex
Gateway as a custom policy and presents one MCP endpoint to clients.
Internally it speaks MCP Streamable HTTP to each upstream, fanning out
the discovery calls (`tools/list`, `resources/list`, `prompts/list`)
and routing the invocation calls (`tools/call`, `resources/read`,
`prompts/get`) to the right backend.

## Features (0.3.x)

- **One endpoint, up to 50 MCP servers.** Clients see a single `/mcp`
  URL and one combined tool catalogue.
- **Declarative configuration in API Manager.** Add, remove or disable
  an upstream MCP server by editing the policy configuration — no code
  changes, no redeploys.
- **Per-server authentication.** Each upstream uses its own scheme:
  `none`, `bearerToken`, `basicAuth`, `apiKeyHeader`, `customHeaders`
  or `passthrough`. Static secrets stay out of clients.
- **Masked secrets.** `token`, `password`, `apiKey` and every
  `headers[].value` are `security:sensitive`, so API Manager masks them.
- **Per-user token passthrough.** `authType: passthrough` forwards the
  caller's `Authorization` header (or `headerName`) to that upstream
  only. `headerName` may list several comma-separated headers, e.g.
  `client_id, client_secret` for a Client ID Enforcement upstream; all
  must be present.
- **Mixed-auth aggregation.** `tools/list` returns everything from the
  upstreams that answered; upstreams that need a token the caller
  didn't send, or rejected it, are skipped.
- **OAuth-ready challenges.** A routed call to a passthrough upstream
  without a valid token gets HTTP 401/403 with `WWW-Authenticate`;
  optional RFC 9728 protected-resource metadata is served by the policy,
  and `requireCallerAuth` challenges callers on connect.
- **Fast discovery.** `*/list` calls query all upstreams concurrently;
  each upstream's answer is cached for `manifestCacheTtlSeconds` (per
  caller for `passthrough` servers).
- **Collision handling.** Duplicate names across upstreams are
  disambiguated automatically (default: prefix with the server name).
- **Spec-compliant MCP Streamable HTTP.** Protocol-version negotiation,
  `notifications/initialized` (202), POST-only transport (`GET` gets
  405), stateless client sessions, tool names matching
  `^[a-zA-Z0-9_-]{1,64}$`.
- **Upstream session management.** Sessions are reused per server (per
  user for passthrough), expire after a TTL and are re-established
  automatically.
- **Operational safety.** Slow, failing or unauthorized upstreams are
  skipped during aggregation; each upstream call has one timeout
  budget, handshake included; answers over `maxUpstreamResponseBytes`
  are refused.
  A failed upstream handshake is backed off for
  `upstreamFailureBackoffSeconds`; upstream 429 / 5xx keep their status
  and `Retry-After`. A misconfigured server entry only takes that server
  out of service; invalid hub-wide settings fail closed (HTTP 503).
- **Trace propagation.** `traceparent`, `tracestate` and
  `x-correlation-id` (configurable with `forwardHeaders`) reach the
  upstream of every routed call. `exposeUpstreamErrors` adds upstream error bodies and the
  names of skipped servers to responses.

The full configuration reference — authentication, mixed-auth
behaviour, collision strategies, settings, methods, limitations and
security notes — is the Exchange page,
[`definition/home.md`](definition/home.md).

Requires Flex Gateway 1.6.1 or later.

## Business benefits

| Benefit | What it means in practice |
|---|---|
| **Faster agent rollout** | Onboard a new MCP-enabled app in minutes by editing API Manager config, instead of pushing connection updates to every client. |
| **One governed front door** | API Manager owns auth, throttling, logging, and policy chaining for the entire MCP fleet — your agents never get raw access to backend MCP servers. |
| **Safe credential boundary** | Static secrets live only in the policy config, in masked (`security:sensitive`) fields — never in client config. Rotating an upstream's token is a policy-config edit. |
| **Vendor-neutral agents** | Any MCP-conformant client works against the hub — no client-specific connectors required. |
| **Pay-as-you-grow tool catalogue** | Add new tool surfaces (a new MCP server) without retraining or reconfiguring agents — the next `tools/list` call surfaces them automatically. |
| **Centralized observability** | Every MCP call traverses Flex Gateway — leverage the platform's existing telemetry instead of stitching logs from N MCP servers. |

---

## How to use it

### 1. Publish the policy to Exchange

```sh
make setup           # one-time: installs cargo-anypoint, fetches deps
make build           # produces wasm + definition + implementation YAMLs
make publish         # publishes a development version to Anypoint Exchange
make release         # publishes a release version
```

This pushes two assets:

- `mcp-hub-policy-dev` — the policy definition (schema)
- `mcp-hub-policy-impl-dev` — the WebAssembly implementation

### 2. Create an **MCP-typed** API instance in API Manager

This policy is designed to be applied to an **MCP API instance**, not
a normal HTTP/REST instance. The hub itself is the MCP endpoint —
the upstream URI on the API instance is **never used**, because the
policy intercepts every request and synthesizes the response.

In API Manager → Add API → Manage API:

| Field | Value |
|---|---|
| API type | **MCP** |
| Asset | any (this represents the virtual hub, not a real backend) |
| Implementation URI | **any URL that resolves** — `https://example.com/`, `https://placeholder.example/`, even `https://anypoint.mulesoft.com` works. The policy never forwards to it. |
| Path / Endpoint URI | the public path you want clients to use (e.g. `/mcp-hub/`) |
| Target | your Flex Gateway target |

> **Why "any resolving URL"?** Flex Gateway requires a syntactically
> valid endpoint URI on the API instance. The hub policy short-circuits
> every recognized request in the request phase (`Flow::Break`), so
> the upstream is never contacted. Use any reachable hostname — common
> choice: `https://anypoint.mulesoft.com`.

### 3. Apply the policy to the instance

In API Manager → your instance → Policies → Apply New Policy → choose
**MCP Hub Policy**, paste a config like:

```yaml
mcpEndpoint:           /mcp
strictMode:            true
collisionStrategy:     prefix
aggregationTimeoutMs:  5000
perRequestTimeoutMs:   30000
manifestCacheTtlSeconds: 60     # 0 = always ask the upstreams
upstreamFailureBackoffSeconds: 10   # 0 = retry a failed upstream on every call
mcpServers:
  - name:      erp
    endpoint:  https://erp-mcp.example.com/mcp
    enabled:   true
    authType:  bearerToken
    token:     <erp-service-token>

  - name:      crm
    endpoint:  https://crm-mcp.example.com/mcp
    enabled:   true
    authType:  apiKeyHeader
    headerName: x-api-key
    apiKey:    <crm-api-key>

  - name:      kb
    endpoint:  https://kb-mcp.example.com/mcp
    enabled:   true
    authType:  none
```

Secret fields — `token`, `password`, `apiKey` and every `headers[].value`
— are marked `security:sensitive` in the policy schema: API Manager masks
them while you type and does not show the stored value again. Enter the
literal secret; the policy does not resolve `${secure::...}` or other
property placeholders. Up to 50 servers can be configured.

Or via CLI:

```sh
anypoint-cli-v4 api-mgr policy apply <apiInstanceId> mcp-hub-policy-dev \
  --policyVersion <published-version> \
  --groupId <yourGroupId> \
  --configFile mcp-hub-config.json

anypoint-cli-v4 api-mgr api redeploy <apiInstanceId>
```

### 4. Connect a client

Tell your MCP client to connect to the published gateway URL plus the
configured `mcpEndpoint`:

```
https://<your-flex-gateway-host>/<api-instance-path>/mcp
```

Examples:

- **Claude Desktop** — add to `claude_desktop_config.json`:
  ```json
  {
    "mcpServers": {
      "company-hub": {
        "url": "https://gateway.example.com/mcp-hub/mcp"
      }
    }
  }
  ```
- **MCP Inspector** — paste the URL, transport **streamable-http**.
- **Postman / curl** — POST JSON-RPC envelopes to the same URL.

### 5. Verify

```sh
URL=https://<your-gateway>/mcp-hub/mcp

# initialize → 200 (stateless: no mcp-session-id)
curl -sSi "$URL" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize",
       "params":{"protocolVersion":"2024-11-05","capabilities":{},
                 "clientInfo":{"name":"probe","version":"0.1"}}}'

# tools/list → aggregated catalogue, names like erp_get_inventory, crm_search
# Add -H "authorization: Bearer $TOKEN" to include passthrough upstreams.
curl -sS "$URL" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | jq .
```

---

## Upgrading from 0.2.x

Existing **policy configurations** (static-auth servers, `mcpEndpoint`,
`strictMode`, collision strategy, timeouts, `diagnosticContinueFlow`)
usually load unchanged. **MCP clients** may see behaviour differences:

| Change | What to do |
|---|---|
| Hub is stateless toward clients — `initialize` no longer issues `mcp-session-id` | Drop any client logic that requires a hub session id. Upstream sessions are still managed by the policy. |
| `protocolVersion` is negotiated; an unsupported `MCP-Protocol-Version` header is HTTP 400 | Send a supported version (`2025-06-18`, `2025-03-26`, or `2024-11-05`), or omit the header. |
| Client notifications are forwarded only to the caller's **passthrough** sessions; `notifications/initialized` and `notifications/cancelled` are not fanned out | Do not rely on the hub relaying client notifications to static-auth upstreams. |
| `manifestCacheTtlSeconds` (default **60**) now caches each upstream's `*/list` answer | Expect up to 60s-stale catalogues. Set `manifestCacheTtlSeconds: 0` if lists must always be live. |
| `diagnosticContinueFlow` is deprecated and ignored | Remove it when convenient; it never changed request flow in 0.2.x either. |

Full notes: [`CHANGELOG.md`](CHANGELOG.md) (0.3.0–0.3.5).

---

## Method dispatch

| MCP method        | Where it's handled                            |
|-------------------|-----------------------------------------------|
| `initialize`      | Local handshake — hub identifies itself       |
| `ping`            | Local 200                                     |
| `tools/list`      | Aggregate across all enabled upstreams        |
| `tools/call`      | Route to the upstream owning the tool name    |
| `resources/list`  | Aggregate                                     |
| `resources/read`  | Route by `uri` to the server that listed it   |
| `prompts/list`    | Aggregate                                     |
| `prompts/get`     | Route                                         |
| `resources/templates/list` | Local, always empty                  |
| `notifications/*` | 202; forwarded best-effort to passthrough upstreams where the caller has a session |
| anything else     | `METHOD_NOT_FOUND`                            |
| `GET /mcp`        | 405 + `Allow: POST` (no server-initiated stream) |
| batches, `id: null`, no `method` | `-32600`; a client JSON-RPC response gets 202 |

## Limits and known limitations

- Up to 50 servers per policy.
- With a cold cache, `*/list` latency is set by the slowest upstream;
  cached answers can be up to `manifestCacheTtlSeconds` old. In a
  50-server test on CloudHub (public MCP servers), a cold `tools/list`
  took about 0.5–5 s and a cached one about 0.1–0.3 s. Each gateway
  replica has its own cache.
- Upstream sessions and the failure backoff are also kept per gateway
  replica: with N replicas a down upstream is tried up to N times per
  `upstreamFailureBackoffSeconds` window.
- Cached `passthrough` lists are not re-checked against the caller's
  token, so a revoked token can still see the list until it expires.
  Routed calls always go to the upstream with the caller's token.
- `resources/read` works for URIs listed by `resources/list`; resource
  templates are not aggregated. Resources on `passthrough` servers are
  found only when the caller sends a token; without one the read returns
  `RESOURCE_NOT_FOUND` (`-32012`), not HTTP 401.
- Only the first page of each upstream's list is returned.
- Responses are buffered, not streamed; progress and log notifications
  an SSE upstream sends before its result are dropped, and
  `notifications/cancelled` is not forwarded (the hub uses its own
  request ids upstream).
- Upstream 429 / 5xx keep their status and `Retry-After`; other upstream
  HTTP errors besides passthrough 401/403 become JSON-RPC `-32010` on
  HTTP 200 (an HTTP 404 would make clients drop their session). The
  upstream body is relayed only with `exposeUpstreamErrors`.
- Only the auth headers and `forwardHeaders` reach upstreams, and
  `forwardHeaders` only on routed calls; upstream response headers other
  than `Retry-After` don't reach the client.
- The gateway buffers each upstream answer before the hub sees it;
  `maxUpstreamResponseBytes` stops the hub parsing and relaying a larger
  one, not the gateway receiving it.
- `passthrough` gives each distinct caller credential its own upstream
  session and cached lists. Put JWT validation and rate limiting in front
  of the hub so unauthenticated callers can't open sessions on your
  upstreams with made-up tokens.
- Static-auth upstreams share one upstream MCP session across callers.
- Non-`Authorization` passthrough (e.g. `client_id, client_secret`):
  the upstream's 401 challenge is relayed; a missing header gives HTTP
  401 without an OAuth challenge and `error.data.missingHeaders`.

## Collision handling

`collisionStrategy` (default `prefix`) controls what happens when two
upstreams declare the same tool / resource / prompt name. With
`prefix` the hub renames inbound to `<server>_<name>` (using `_` so
the names satisfy the MCP-client tool-name regex
`^[a-zA-Z0-9_-]{1,64}$`) and rewrites outbound to the original name —
clients see disambiguated names, upstreams see their native namespaces.

Other modes: `reject` (fail aggregation on duplicate), `firstWins`,
`lastWins`.

## Layout

```
mcp-hub-policy/
├── definition/
│   ├── gcl.yaml          # Policy schema published to Anypoint Exchange
│   └── home.md           # Exchange home page (configuration reference)
├── playground/           # Local Flex Gateway for `make run`
├── src/
│   ├── lib.rs            # entrypoint, request_filter, dispatch
│   ├── config.rs         # typed PolicyConfig + validation
│   ├── jsonrpc.rs        # JSON-RPC 2.0 envelope + MCP error codes
│   ├── upstream.rs       # outbound JSON-RPC client per MCP server
│   ├── session.rs        # upstream MCP session cache
│   ├── manifest.rs       # upstream */list answer cache
│   ├── mcp/
│   │   ├── method.rs     # method classifier
│   │   ├── types.rs      # serverInfo, capabilities
│   │   └── aggregate.rs  # collision-aware manifest merge
│   └── generated/        # codegen mirror (auto-regen by `make build`)
└── tests/                # integration tests (with the unit tests in src/: 131)
```

## Build / test / publish

```sh
make setup     # one-time
make build     # builds wasm + definition + implementation YAMLs
make test      # cargo unit + integration tests
make run       # local Flex Gateway with the policy applied
make publish   # development version → Anypoint Exchange
make release   # release version → Anypoint Exchange
```

### Release flow

1. Bump `version` in `Cargo.toml` and add a `CHANGELOG.md` entry.
   Exchange versions are immutable: a published version can't be
   republished.
2. Run `make test` and `make build`.
3. Run `make release` to publish the definition and implementation assets.
4. Run `make upload-docs` to upload `definition/home.md` as the
   Exchange home page. Exchange shares the page across a minor version
   (all 0.3.x releases show the same page).

`make build` regenerates `src/generated/config.rs` from
`definition/gcl.yaml`. Don't edit the generated file by hand — change
the GCL.

## Roadmap & changelog

- [`CHANGELOG.md`](CHANGELOG.md) — version-by-version release notes.
- [`ROADMAP.md`](ROADMAP.md) — items deferred from v1 and future
  directions (streaming pass-through, capability tokens, mTLS, multi-tenant
  isolation, etc.).
