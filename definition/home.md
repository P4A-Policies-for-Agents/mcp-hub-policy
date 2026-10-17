# MCP Hub Policy
_Single endpoint for multiple MCP Servers_

A custom **MuleSoft Flex Gateway / Omni Gateway** policy that turns multiple MCP servers into one **virtual MCP aggregation endpoint**. AI agents and MCP clients connect to a single `/mcp` URL; the policy talks to every configured upstream MCP server, merges their tool, resource and prompt manifests, applies per-server authentication — including forwarding the end user's own token where an upstream needs it — and routes each invocation to the server that owns it.

Built with [PDK 1.8](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview) (Rust → WebAssembly). Requires Flex Gateway 1.6.1 or later.

---

## Purpose

Model Context Protocol (MCP) is the emerging standard that LLM-based agents (Claude Desktop, Cursor, Cline, MCP Inspector, ChatGPT Desktop, ServiceNow's agent runtime, etc.) use to discover and invoke tools. Each enterprise MCP server exposes its own catalogue, its own auth and its own URL. As soon as you have more than one — an ERP server, a CRM server, a knowledge-base server — every client needs a separate connection per server, and every server has to be exposed individually through your gateway.

`mcp-hub-policy` removes both problems. It runs inside Flex Gateway as a custom policy and presents one MCP endpoint to clients. Internally it speaks MCP Streamable HTTP to each upstream, fanning out the discovery calls (`tools/list`, `resources/list`, `prompts/list`) and routing the invocation calls (`tools/call`, `resources/read`, `prompts/get`) to the right backend.

## Features

- **One endpoint, up to 50 MCP servers.** Clients see a single `/mcp` URL and one combined tool catalogue.
- **Declarative configuration in API Manager.** Add, remove or disable an upstream MCP server by editing the policy configuration — no code changes, no redeploys.
- **Per-server authentication.** Each upstream uses its own scheme: `none`, `bearerToken`, `basicAuth`, `apiKeyHeader`, `customHeaders` or `passthrough`. Static secrets stay out of clients.
- **Masked secrets.** `token`, `password`, `apiKey` and every `headers[].value` are `security:sensitive`: API Manager masks them while you type and does not show the stored value again.
- **Per-user token passthrough.** `authType: passthrough` forwards the caller's `Authorization` header (or the header named in `headerName`) to that upstream only, so JWT-secured MCP servers authorize each end user with their own identity.
- **Mixed-auth aggregation.** `tools/list` returns everything from the upstreams that answered; upstreams that need a token the caller didn't send, or rejected it, are skipped.
- **OAuth-ready challenges.** A routed call to a passthrough upstream without a valid token returns HTTP 401/403 with `WWW-Authenticate`, so MCP clients can start their OAuth flow. Optional protected-resource metadata (RFC 9728) is served by the policy.
- **Fast discovery.** `*/list` calls query all upstreams concurrently, and each upstream's answer is cached for `manifestCacheTtlSeconds` (per caller for `passthrough` servers).
- **Collision handling.** Two upstreams advertising the same name are disambiguated automatically (default: prefix with the server name).
- **Spec-compliant MCP Streamable HTTP.** Protocol-version negotiation, `notifications/initialized` (202), `GET` for SSE clients, stateless client sessions, and tool names that match `^[a-zA-Z0-9_-]{1,64}$`.
- **Upstream session management.** Upstream MCP sessions are reused per server (and per user for passthrough), expire after a TTL, and are re-established automatically when an upstream forgets them.
- **Operational safety.** Slow, failing or unauthorized upstreams are skipped during aggregation; routed calls have per-request timeouts; errors are returned as MCP-compliant JSON-RPC errors or, for authentication, HTTP 401/403.

## Business benefits

| Benefit | What it means in practice |
|---|---|
| **Faster agent rollout** | Onboard a new MCP-enabled app in minutes by editing API Manager config, instead of pushing connection updates to every client. |
| **One governed front door** | API Manager owns auth, throttling, logging and policy chaining for the entire MCP fleet — your agents never get raw access to backend MCP servers. |
| **Safe credential boundary** | Static secrets live only in the policy config, in masked fields — never in client config. User tokens reach only the upstreams configured for `passthrough`. |
| **User-level authorization** | JWT-secured MCP servers authorize each end user with their own token instead of a shared service credential. |
| **Vendor-neutral agents** | Any MCP-conformant client works against the hub — no client-specific connectors required. |
| **Pay-as-you-grow tool catalogue** | Add a new MCP server without reconfiguring agents — the next `tools/list` call surfaces its tools automatically. |
| **Centralized observability** | Every MCP call traverses Flex Gateway — use the platform's existing telemetry instead of stitching logs from N MCP servers. |

---

## Set up

1. In API Manager create an API instance with **type = MCP** on your Flex Gateway. The hub itself is the MCP endpoint: the instance's upstream URI is never called, so any reachable URL satisfies API Manager's validation (for example `https://anypoint.mulesoft.com`).
2. Apply **MCP Hub Policy** to the instance with a configuration like the one below.
3. Point MCP clients at `https://<gateway-host>/<api-path>/mcp` — for MCP Inspector use transport **streamable-http**.

## Configuration

```yaml
mcpEndpoint:          /mcp
strictMode:           true
collisionStrategy:    prefix            # prefix | reject | firstWins | lastWins

# Only needed when using authType: passthrough
resourceUrl:          https://<gateway-host>/<api-path>/mcp
authorizationServers: [https://<your-idp>/oauth2/<server-id>]
scopesSupported:      []                # optional

mcpServers:
  - name:     kb                        # public server, no auth
    endpoint: https://kb-mcp.example.com/mcp
    enabled:  true
    authType: none

  - name:     erp                       # static service credential
    endpoint: https://erp-mcp.example.com/mcp
    enabled:  true
    authType: bearerToken
    token:    <erp-service-token>       # masked in API Manager

  - name:     escalate                  # validates the end user's JWT
    endpoint: https://<gateway-host>/devops-escalation-mcp/mcp
    enabled:  true
    authType: passthrough               # forwards the caller's Authorization
```

Each `mcpServers` entry needs a unique `name` and an `endpoint`. Use letters, digits, `_` and `-` in names, and avoid names where one is the other plus `_…` (for example `jira` and `jira_dev`), because the name becomes the tool prefix.

### Per-server authentication

| `authType`      | Required fields          | Header(s) sent to the upstream |
|-----------------|--------------------------|--------------------------------|
| `none`          | —                        | none |
| `bearerToken`   | `token`                  | `Authorization: Bearer <token>` |
| `basicAuth`     | `username`, `password`   | `Authorization: Basic …` |
| `apiKeyHeader`  | `headerName`, `apiKey`   | `<headerName>: <apiKey>` |
| `customHeaders` | `headers` array          | every entry of `headers` |
| `passthrough`   | — (optional `headerName`)| the caller's `<headerName>` (default `Authorization`), unchanged |

Enter static secrets as literal values; the policy does not resolve `${secure::...}` or other property placeholders.

The `headers` array is always added on top of the auth scheme — use it for tenant headers, environment markers or other metadata. For `passthrough`, a `headers` entry with the same name as the passthrough header overrides the caller's value. Transport headers (`host`, `content-type`, `mcp-session-id`, `mcp-protocol-version`, …) cannot be passed through.

### Mixed authentication — what clients see

| Request | Caller sends a valid token | Caller sends no / invalid token |
|---|---|---|
| `initialize`, `ping` | 200 | 200 |
| `tools/list` (and other `*/list`) | entries from **all** upstreams | entries from upstreams that don't need the caller's token |
| `tools/call` → static / no-auth upstream | routed | routed |
| `tools/call` → `passthrough` upstream | routed with the caller's token | **HTTP 401** + `WWW-Authenticate` |

When `authorizationServers` is configured, `WWW-Authenticate` points at the metadata the policy serves at `<resourceUrl>/.well-known/oauth-protected-resource`; otherwise the upstream's own challenge is relayed.

### Collision strategies

| Strategy | Behaviour |
|---|---|
| `prefix` (default) | Rename to `<server>_<name>`; both entries are kept. `tools/call` accepts the prefixed name and sends the original name upstream. |
| `reject` | Fail aggregation with an `INTERNAL_ERROR` JSON-RPC error. |
| `firstWins` | Keep the first server's entry, drop later duplicates. |
| `lastWins` | Keep the last server's entry. |

Servers are ordered by `priority` (lower first), then by configuration order; with `firstWins` / `lastWins` this decides which server wins.

### Other settings

| Property | Default | Purpose |
|---|---|---|
| `mcpEndpoint` | `/mcp` | Path the hub serves, matched as a suffix — if the API is mounted at `/api/v1`, the hub answers at `/api/v1/mcp`. |
| `strictMode` | `true` | `true`: requests that don't hit `mcpEndpoint` get 404. `false`: they continue to the API's upstream, so the policy can share an instance. |
| `aggregationTimeoutMs` | `5000` | Timeout for each upstream's `*/list` call (upstreams are queried concurrently). Setting up a new upstream session uses `perRequestTimeoutMs`. |
| `perRequestTimeoutMs` | `30000` | Timeout for routed calls; override per server with `timeoutMs`. |
| `upstreamSessionTtlSeconds` | `1800` | How long an upstream MCP session is reused (per server, per user for `passthrough`). `0` disables reuse. |
| `allowedOrigins` | empty | If set, requests whose `Origin` is not listed get HTTP 403. Empty = rely on a gateway CORS policy. |
| `manifestCacheTtlSeconds` | `60` | How long each upstream's `*/list` answer is reused (per server, per user for `passthrough`). Failed answers and answers over 64 KB are not cached. `0` disables the cache. |
| `diagnosticContinueFlow` | `false` | Deprecated and ignored. |

## Methods served

| JSON-RPC method | Behaviour |
|---|---|
| `initialize` | Answered by the hub; negotiates `protocolVersion` (`2025-06-18`, `2025-03-26`, `2024-11-05`). Stateless: no `mcp-session-id` is issued. |
| `ping` | Answered by the hub. |
| `tools/list`, `resources/list`, `prompts/list` | Aggregated from every enabled upstream, with the collision strategy applied. |
| `tools/call`, `prompts/get` | Routed to the upstream that owns the name. |
| `resources/read` | Routed to the upstream whose `resources/list` contains the `uri`. |
| `notifications/*` | HTTP 202; forwarded best-effort to passthrough upstreams where the caller has a session. |
| anything else | `METHOD_NOT_FOUND` JSON-RPC error. |

`GET /mcp` with `Accept: text/event-stream` gets an empty SSE stream; other `GET`s get 405. An unsupported `MCP-Protocol-Version` header gets HTTP 400. The hub advertises only the `tools`, `resources` and `prompts` capabilities.

## Upgrading from 0.2.x

Existing **policy configurations** (static-auth servers, `mcpEndpoint`, `strictMode`, collision strategy, timeouts, `diagnosticContinueFlow`) usually load unchanged. **MCP clients** may see behaviour differences:

| Change | What to do |
|---|---|
| Hub is stateless toward clients — `initialize` no longer issues `mcp-session-id` | Drop any client logic that requires a hub session id. Upstream sessions are still managed by the policy. |
| `protocolVersion` is negotiated; an unsupported `MCP-Protocol-Version` header is HTTP 400 | Send a supported version (`2025-06-18`, `2025-03-26`, or `2024-11-05`), or omit the header. |
| Client notifications are forwarded only to the caller's **passthrough** sessions; `notifications/initialized` and `notifications/cancelled` are not fanned out | Do not rely on the hub relaying client notifications to static-auth upstreams. |
| `manifestCacheTtlSeconds` (default **60**) now caches each upstream's `*/list` answer | Expect up to 60s-stale catalogues. Set `manifestCacheTtlSeconds: 0` if lists must always be live. |
| `diagnosticContinueFlow` is deprecated and ignored | Remove it when convenient; it never changed request flow in 0.2.x either. |

Full release notes are in `CHANGELOG.md` (0.3.0–0.3.2) in the source repository.

## Limits and known limitations

- **Up to 50 servers** per policy.
- **Discovery latency** is set by the slowest upstream when the cache is cold, and is fast while cached answers are fresh. In a 50-server test on CloudHub (public MCP servers), a cold `tools/list` took about 0.5–5 s and a cached one about 0.1–0.3 s. Each gateway replica has its own cache, so a request served by another replica can be slow again.
- **Cached lists can be up to `manifestCacheTtlSeconds` old.** A tool added or removed on an upstream appears after the cached answer expires; set `0` if lists must always be live. Cached `passthrough` lists are served without asking the upstream to re-check the caller's token, so a revoked token can still see the list until it expires; routed calls always go to the upstream with the caller's token.
- **`resources/read`** works for URIs that appear in an upstream's `resources/list`. Resource templates (`resources/templates/list`) are not aggregated. If two servers list the same URI, the first by `priority` receives the read. Resources on `passthrough` servers are found only when the caller sends a token; without one the read returns `RESOURCE_NOT_FOUND` (`-32012`), not HTTP 401, so it does not start the client's OAuth login.
- **Pagination:** only the first page of each upstream's list is returned.
- **SSE upstreams:** responses are buffered, not streamed. Progress and log notifications an upstream sends before its result are dropped.

## Security notes

- **Token passthrough** sends the caller's token unchanged. Each `passthrough` upstream must accept tokens issued for the hub, and any of them could replay a token against the others. The MCP authorization specification discourages token passthrough; use it when the hub and its upstreams share an authorization server and trust boundary.
- The hub does not validate inbound tokens itself. To require authentication for **every** request (including `initialize` and `tools/list`), apply a JWT Validation policy ahead of this policy.
- Logging policies that record request headers will record user tokens; exclude `Authorization` from logged headers.
- Upstream error messages are relayed to clients and may contain internal host names.

---

## Details

- **Category** — MCP
- **Maintainer** — Amir Khan
- **Version** — 0.3.x
- **Release notes** — `CHANGELOG.md` in the source repository

## Links

- **Source repository** — https://github.com/P4A-Policies-for-Agents/mcp-hub-policy
- **Video tutorial** — https://youtu.be/3wY2t23moCc?si=HQPDpe2bYXY7NorY
