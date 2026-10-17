# Backlog — pick up tomorrow

Status as of end of day 2026-05-21.

## Where we left off

Policy v0.2.0 is **published** to Exchange and **applied** to a deployed
Flex Gateway API instance, but the live endpoint returns
`503 no healthy upstream` on `POST /mcp` even though the same endpoint
returns 200 from the hub-info path on `GET /mcp`. The policy is
intercepting GET correctly (we see `mcp-hub-policy 0.2.0` JSON come
back) but the POST path is being short-circuited by Flex Gateway's
upstream-cluster check before the policy's `response_filter` can call
`send_response`.

The unit + integration test suite (32 tests) all pass; the policy logic
is correct in isolation. The blocker is gateway-side: how Flex Gateway
routes a POST when no upstream cluster is healthy.

## Demo artifacts

- **Exchange**:
  - Definition: `mcp-hub-policy-dev` v0.2.0-20260521155125
    (https://anypoint.mulesoft.com/exchange/f684513f-280e-403a-ac53-80ca62e3de49/mcp-hub-policy-dev)
  - Implementation: `mcp-hub-policy-impl-dev` v0.2.0-20260521155125
- **API instance**: id `20922804`, label `MCP Hub Demo`
  - public URL: `https://agent-network-ingress-gw-b2jb0y.1d6nel.usa-e1.cloudhub.io/mcp-hub-demo/mcp`
  - upstream URI: `https://mcp-server-demo-erp-cqhd4c.i82zax.usa-e2.cloudhub.io/`
  - target: `agent-network-ingress-gw` (29e9e3ad-0efd-4eef-aa5c-0b8cfcb72519)
  - policy id `8318857` enabled, configured with three upstreams:
    - `erp` → `https://mcp-server-demo-erp-cqhd4c.i82zax.usa-e2.cloudhub.io/mcp`
    - `crm` → `https://mcp-server-demo-crm-b2jb0y.5sc6y6-1.usa-e2.cloudhub.io/mcp`
    - `agent-network-erp` → `https://agent-network-ingress-gw-b2jb0y.1d6nel.usa-e1.cloudhub.io/mcp-erp-tool/`

## To-do, prioritized

### 1. Fix the 503 on POST /mcp (BLOCKER for the demo)

Symptom: `GET /mcp` returns the hub-info JSON (proves policy is loaded
and routing). `POST /mcp` returns `503 no healthy upstream` from
`server: Anypoint Flex Gateway`.

Likely causes (debug in this order):

- **`Flow::Continue` lets Flex try to forward**. Our `request_filter`
  returns `Flow::Continue(McpPending::Rpc)` on a recognized POST so the
  body-state transition (`request.into_body_state()`) can complete; the
  intent is that `response_filter` calls `send_response` to replace the
  upstream's response. But if Flex emits 503 *before* the policy's
  response phase runs, `send_response` may never be invoked.
  - `acp-to-a2a` uses the same pattern and works in its playground —
    but its playground points the upstream at httpbin (a real,
    reachable host), so the upstream cluster is "healthy" and a real
    response comes back which the policy then overrides.
  - **Quickest fix to verify**: switch the request handler so when the
    method is recognized as MCP, we collect the body inline and
    `Flow::Break` immediately with the dispatched response, skipping
    the response phase entirely. Equivalent to the `request_filter`
    short-circuiting today's `HubInfo` and `PassThrough` cases.
  - Code pointer: `src/lib.rs:request_filter` — replace the
    `Flow::Continue(McpPending::Rpc { ... })` branch with an inline
    `dispatch_rpc(...).await` and return `Flow::Break(resp)`.

- **The placeholder upstream**. The API was created with
  `--uri https://placeholder-not-used.example.com/`, which Envoy
  resolves to nothing → "no healthy upstream". Even if we move
  dispatch to the request phase, this should still be cleaned up:
  point `--uri` at one of the real MCP backends (already done — pointed
  at the ERP cloudhub URL — so this is no longer the cause).

- **API instance routing rules**. The current API has no `--routing`
  override. The default route maps everything to the implementation
  URI. Could try forcing the policy to fire by setting up an
  explicit routing rule with no upstream, but the cleaner fix is the
  `Flow::Break` change above.

### 2. End-to-end verification once #1 is fixed

```sh
# From a shell with the gateway reachable:
curl -sS https://agent-network-ingress-gw-b2jb0y.1d6nel.usa-e1.cloudhub.io/mcp-hub-demo/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"mcp-inspector","version":"1.0"}}}' | jq .

curl -sS https://agent-network-ingress-gw-b2jb0y.1d6nel.usa-e1.cloudhub.io/mcp-hub-demo/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | jq .
```

Expected: aggregated tools from all three upstreams, prefixed by
server name (`erp_get_inventory`, `erp_submit_order`, … — `_` separator
since 0.2.3). No client `mcp-session-id` is required (hub is
stateless toward clients since 0.3.0).

### 3. Upstream session-id sharing across replicas — shipped in 0.3.0

Upstream `mcp-session-id` values now live in the PDK shared cache
(per server, and per caller for `passthrough`), with
`upstreamSessionTtlSeconds` and re-initialize on upstream 404. Each
replica still has its own **manifest** cache; that remains expected.

### 4. Cap-token / route-trust mode (parity with `acp-to-a2a`)

Tracked in `ROADMAP.md` (P1 #3 / P2 #6). Not for tomorrow.

### 5. Streaming response pass-through

Tracked in `ROADMAP.md` (P1 #1). The current SSE handling in
`upstream::extract_jsonrpc_from_sse` works for one-shot payloads; truly
streaming `tools/call` responses need a different code path on the
inbound side.

## Known good state

```sh
cd /Users/amir.khan/Documents/cursor-workspaces/mcp-hub-policy
cargo test                # 32 passing
make build                # builds wasm + definition
# already published; re-publish only if code changes
make publish
```

## Useful commands for tomorrow

```sh
# Show the deployed API
anypoint-cli-v4 api-mgr api describe 20922804 -o json

# Show applied policies
anypoint-cli-v4 api-mgr policy list 20922804 -o json

# Edit the policy config (config.json contains the mcpServers payload):
anypoint-cli-v4 api-mgr policy edit 20922804 8318857 --configFile /tmp/mcp-hub-demo-config.json

# Redeploy after policy version bumps:
anypoint-cli-v4 api-mgr api redeploy 20922804 -o json

# Probe one of the live MCP servers directly:
curl -sSi -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0.1"}}}' \
  --max-time 15 \
  https://mcp-server-demo-erp-cqhd4c.i82zax.usa-e2.cloudhub.io/mcp
```

## Files modified between yesterday's stopping point and today

- `src/lib.rs` — added per-upstream session-id cache and lazy init
- `src/upstream.rs` — added SSE response parsing + session-id header
  forward + `UpstreamCallResult` type
- `Cargo.toml` — version 0.1.0 → 0.2.0

## Open questions / resolved

- ~~Should the hub minted session id be deterministic…~~ **Resolved in
  0.3.0:** the hub is stateless toward clients (no `mcp-session-id`
  issued). Upstream sessions live in the PDK shared cache.
- ~~Do we want `tools/list` results cached…~~ **Resolved in 0.3.2:**
  `manifestCacheTtlSeconds` caches each upstream's `*/list` answer
  (per caller for passthrough). Aggregated second-level cache is
  optional follow-up in `ROADMAP.md`.
- Confirm we want `--isFlex` API instances to forbid the upstream
  health check when a policy fully short-circuits the request. May
  warrant an MUL ticket if this is gateway-level rather than a policy
  bug.
