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

## Goals

- **One endpoint, many MCP servers.** Clients see a single
  `/mcp` URL and a unified tool catalogue.
- **Declarative configuration in API Manager.** Add or remove an
  upstream MCP server by editing policy config — no code changes, no
  redeploys.
- **Per-server authentication.** Each upstream picks its own scheme
  (`bearerToken`, `basicAuth`, `apiKeyHeader`, `customHeaders`, or
  `none`); the hub keeps secrets out of clients.
- **Spec-compliant MCP Streamable HTTP.** Works out of the box with
  Claude Desktop, MCP Inspector, Postman, ChatGPT Desktop, and any
  conformant Streamable-HTTP client — including correct handling of
  `initialize`, `notifications/initialized` (202), `GET` for SSE
  fallback, and tool-name regex `^[a-zA-Z0-9_-]{1,64}$`.
- **Collision handling.** Two upstreams advertising the same tool name
  are disambiguated automatically (default: prefix with the server
  name).
- **Operational safety.** Slow / failing upstreams are skipped during
  aggregation; routed calls have per-request timeouts; errors are
  returned as MCP-compliant JSON-RPC error envelopes.

## Business benefits

| Benefit | What it means in practice |
|---|---|
| **Faster agent rollout** | Onboard a new MCP-enabled app in minutes by editing API Manager config, instead of pushing connection updates to every client. |
| **One governed front door** | API Manager owns auth, throttling, logging, and policy chaining for the entire MCP fleet — your agents never get raw access to backend MCP servers. |
| **Safe credential boundary** | Secrets stay in API Manager (`${secure::...}`), never in client config. Rotating an upstream's token is a policy-config edit. |
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
manifestCacheTtlSeconds: 60
mcpServers:
  - name:      erp
    endpoint:  https://erp-mcp.example.com/mcp
    enabled:   true
    authType:  bearerToken
    token:     ${secure::erp.token}

  - name:      crm
    endpoint:  https://crm-mcp.example.com/mcp
    enabled:   true
    authType:  apiKeyHeader
    headerName: x-api-key
    apiKey:    ${secure::crm.key}

  - name:      kb
    endpoint:  https://kb-mcp.example.com/mcp
    enabled:   true
    authType:  none
```

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

# initialize → 200 + minted mcp-session-id
curl -sSi "$URL" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize",
       "params":{"protocolVersion":"2024-11-05","capabilities":{},
                 "clientInfo":{"name":"probe","version":"0.1"}}}'

# tools/list → aggregated catalogue, names like erp_get_inventory, crm_search
curl -sS "$URL" -H 'content-type: application/json' \
  -H "mcp-session-id: <id-from-initialize>" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | jq .
```

---

## Method dispatch

| MCP method        | Where it's handled                            |
|-------------------|-----------------------------------------------|
| `initialize`      | Local handshake — hub identifies itself       |
| `ping`            | Local 200                                     |
| `tools/list`      | Aggregate across all enabled upstreams        |
| `tools/call`      | Route to the upstream owning the tool name    |
| `resources/list`  | Aggregate                                     |
| `resources/read`  | Route                                         |
| `prompts/list`    | Aggregate                                     |
| `prompts/get`     | Route                                         |
| `notifications/*` | Best-effort fan-out to every upstream → 202   |
| `GET /mcp`        | 200 + empty `text/event-stream` for SSE clients; otherwise 405 + `Allow: POST` |

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
│   └── home.md           # Exchange landing page
├── playground/           # Local Flex Gateway for `make run`
├── src/
│   ├── lib.rs            # entrypoint, request_filter, dispatch
│   ├── config.rs         # typed PolicyConfig + validation
│   ├── jsonrpc.rs        # JSON-RPC 2.0 envelope + MCP error codes
│   ├── upstream.rs       # outbound JSON-RPC client per MCP server
│   ├── mcp/
│   │   ├── method.rs     # method classifier
│   │   ├── types.rs      # serverInfo, capabilities
│   │   └── aggregate.rs  # collision-aware manifest merge
│   └── generated/        # codegen mirror (auto-regen by `make build`)
└── tests/                # 34 unit + integration tests
```

## Build / test / publish

```sh
make setup     # one-time
make build     # builds wasm + definition + implementation YAMLs
make test      # cargo unit + integration tests (34 passing)
make run       # local Flex Gateway with the policy applied
make publish   # development version → Anypoint Exchange
make release   # release version → Anypoint Exchange
```

`make build` regenerates `src/generated/config.rs` from
`definition/gcl.yaml`. Don't edit the generated file by hand — change
the GCL.

## Roadmap & changelog

- [`CHANGELOG.md`](CHANGELOG.md) — version-by-version release notes.
- [`ROADMAP.md`](ROADMAP.md) — items deferred from v1 and future
  directions (streaming pass-through, capability tokens, mTLS, multi-tenant
  isolation, etc.).
