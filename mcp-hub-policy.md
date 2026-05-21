# Custom Omni Gateway MCP Aggregation Policy — Requirements Specification

## Overview

This document defines the functional and technical requirements for a custom policy built using the [MuleSoft Flex Gateway Policy Development Kit (PDK)](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview).

The policy will run on MuleSoft Omni Gateway / Flex Gateway and provide a **virtual MCP aggregation endpoint** exposed as:

```text
/mcp
```

The policy aggregates multiple upstream MCP servers (Model Context Protocol servers) using the MCP Streamable HTTP transport model.

The upstream configured in API Manager is not relevant because the policy itself acts as the MCP endpoint provider and orchestrator.

The policy is intended for deployment on MCP-enabled APIs/assets managed through MuleSoft API Manager.

---

# 1. Purpose

The policy enables administrators to:

- Configure multiple MCP servers centrally in API Manager
- Expose a single MCP endpoint to AI agents/clients
- Dynamically aggregate MCP capabilities from multiple upstream servers
- Standardize authentication handling
- Route MCP tool/resource requests to the correct backend MCP server
- Support MCP Streamable HTTP semantics

---

# 2. Goals

## Primary Goals

- Provide a unified `/mcp` endpoint
- Support multiple MCP upstream servers
- Support MCP Streamable HTTP transport
- Enable declarative configuration in API Manager
- Support per-server authentication
- Support per-server custom headers
- Preserve MCP protocol semantics
- Support stateless operation where possible

## Non-Goals

- Acting as a generic API proxy
- Persisting MCP session state externally
- Transforming MCP tool payloads
- Implementing AI orchestration logic
- Managing upstream lifecycle

---

# 3. High-Level Architecture

```text
AI Agent / MCP Client
        |
        v
+----------------------+
| Omni Gateway Policy  |
|   Virtual /mcp       |
+----------------------+
      |       |       |
      v       v       v
   MCP A   MCP B   MCP C
```

The policy intercepts requests on `/mcp` and internally routes them to configured upstream MCP servers.

---

# 4. Platform Requirements

## Runtime

- MuleSoft Omni Gateway / Flex Gateway
- Connected Mode deployment
- API Manager managed policy
- Rust-based PDK implementation

## Development Framework

The implementation must use the MuleSoft Flex Gateway Policy Development Kit (PDK) and follow the PDK reactor/executor architecture.

## Policy Type

- Custom Policy
- Request/Response interception policy
- MCP-aware policy

---

# 5. MCP Protocol Requirements

## Supported MCP Transport

The policy must support:

- MCP Streamable HTTP transport
- HTTP POST requests
- SSE-compatible streaming responses where required
- `/mcp` single endpoint style

## Supported MCP Operations

The policy must support pass-through and routing for:

- `initialize`
- `tools/list`
- `tools/call`
- `resources/list`
- `resources/read`
- `prompts/list`
- `prompts/get`
- notifications
- streaming responses

---

# 6. Functional Requirements

# 6.1 Virtual MCP Endpoint

The policy must expose:

```text
/mcp
```

regardless of configured upstream API implementation.

The policy becomes the effective MCP endpoint.

---

# 6.2 MCP Server Registry

The policy configuration must support an array of MCP server definitions.

## API Manager UI Behavior

Administrators configure MCP servers using an "Add" button pattern.

Each added item represents one MCP server.

---

# 6.3 MCP Server Configuration Model

Each MCP server entry must contain:

| Field | Type | Required | Description |
|---|---|---|---|
| name | string | yes | Logical server identifier |
| endpoint | string | yes | MCP Streamable HTTP endpoint |
| enabled | boolean | yes | Enables/disables server |
| authType | enum | yes | Authentication strategy |
| headers | array | no | Additional HTTP headers |
| timeoutMs | integer | no | Request timeout |
| priority | integer | no | Optional routing priority |

---

# 6.4 Header Configuration

Each MCP server may contain an array of headers configured line-by-line.

Example:

| Header Name | Header Value |
|---|---|
| x-api-key | abc123 |
| x-env | prod |

The UI must support dynamic addition/removal of header entries.

---

# 6.5 Authentication Configuration

The policy must support per-server authentication selection via dropdown.

## Supported Authentication Types

| Type | Description |
|---|---|
| none | No authentication |
| bearerToken | Static bearer token |
| basicAuth | Username/password |
| apiKeyHeader | API key in custom header |
| customHeaders | Arbitrary auth headers |

---

# 6.6 Authentication Configuration Fields

## bearerToken

| Field | Required |
|---|---|
| token | yes |

---

## basicAuth

| Field | Required |
|---|---|
| username | yes |
| password | yes |

---

## apiKeyHeader

| Field | Required |
|---|---|
| headerName | yes |
| apiKey | yes |

---

## customHeaders

Uses configured headers array.

---

# 6.7 MCP Tool Aggregation

The policy must aggregate tools from all configured MCP servers.

## Requirements

- Call `tools/list` on upstream servers
- Merge tool definitions
- Preserve source server mapping
- Return unified tool list to client

---

# 6.8 Tool Name Collision Handling

The policy must support configurable collision behavior.

## Collision Strategies

| Strategy | Behavior |
|---|---|
| prefix | Prefix tool names with server name |
| reject | Reject duplicate tool names |
| firstWins | Keep first discovered tool |
| lastWins | Overwrite previous tool |

Default:

```text
prefix
```

Example:

```text
jira.search
slack.search
```

---

# 6.9 Tool Invocation Routing

When a client invokes a tool:

1. Policy identifies owning MCP server
2. Policy forwards request
3. Policy streams response back to client
4. Policy preserves MCP payloads

---

# 6.10 Resource Aggregation

The policy should optionally support:

- `resources/list`
- `resources/read`

using the same aggregation/routing behavior.

---

# 6.11 Prompt Aggregation

The policy should optionally support:

- `prompts/list`
- `prompts/get`

---

# 6.12 Streaming Support

The policy must support:

- `text/event-stream`
- chunked responses
- long-running MCP responses
- bidirectional MCP stream semantics where supported

---

# 6.13 Error Handling

The policy must gracefully handle:

| Scenario | Behavior |
|---|---|
| MCP server offline | Partial degradation |
| Timeout | MCP error response |
| Invalid auth | Configured server marked unavailable |
| Invalid JSON-RPC | Return MCP-compliant error |
| Stream interruption | Close stream cleanly |

---

# 7. Non-Functional Requirements

# 7.1 Performance

| Metric | Target |
|---|---|
| Tool aggregation startup | < 5 sec |
| Per-request overhead | < 20 ms |
| Concurrent streams | configurable |
| Memory footprint | minimal |

---

# 7.2 Scalability

The policy must support:

- 100+ tools aggregated
- 20+ MCP servers
- concurrent MCP sessions

---

# 7.3 Security

The policy must:

- Never log secrets
- Mask credentials in logs
- Support TLS upstream connections
- Validate HTTPS certificates
- Support secure secret injection

---

# 7.4 Observability

The policy should emit:

| Metric | Description |
|---|---|
| request_count | Total MCP requests |
| upstream_latency | Per-server latency |
| tool_calls | Tool invocation count |
| upstream_errors | Error count |
| stream_disconnects | Streaming interruptions |

---

# 7.5 Logging

Structured logs should include:

- MCP method
- target MCP server
- latency
- status
- correlation ID

---

# 8. API Manager Configuration Schema

Example conceptual configuration:

```yaml
mcpServers:
  - name: jira
    endpoint: https://jira.company.com/mcp
    authType: bearerToken
    token: ${secure::jira.token}
    headers:
      - name: x-env
        value: prod

  - name: slack
    endpoint: https://slack.company.com/mcp
    authType: apiKeyHeader
    headerName: x-api-key
    apiKey: ${secure::slack.key}
```

---

# 9. Policy Lifecycle Requirements

The implementation must support:

- Local testing
- Exchange publishing
- API Manager application
- Versioned policy deployment

---

# 10. Recommended Internal Components

## Suggested Rust Modules

| Module | Responsibility |
|---|---|
| config | Policy configuration parsing |
| auth | Authentication handling |
| registry | MCP server registry |
| aggregation | Tool/resource aggregation |
| routing | Request routing |
| streaming | SSE/stream handling |
| observability | Metrics/logging |

---

# 11. Recommended PDK Features

The implementation should leverage:

- HTTP call support
- async execution
- streaming payload support
- shared state where needed
- request/response hooks

---

# 12. Future Enhancements

Potential future capabilities:

- Dynamic MCP discovery
- Health-check polling
- Circuit breaker support
- Load balancing
- Tool-level authorization
- Multi-tenant isolation
- OAuth2 client credentials
- OpenTelemetry tracing
- Caching of tool manifests
- Session affinity
- Retry policies

---

# 13. Acceptance Criteria

The policy is considered complete when:

- A policy can be applied in API Manager
- Multiple MCP servers can be configured
- `/mcp` endpoint becomes available
- Aggregated `tools/list` works
- `tools/call` routes correctly
- Streaming responses function correctly
- Authentication is applied per upstream
- Duplicate tool handling works
- Logs and metrics are emitted
- Errors are MCP-compliant

---

# 14. References

- Flex Gateway PDK Overview
  - https://docs.mulesoft.com/pdk/latest/policies-pdk-overview

- PDK Architecture Overview
  - https://docs.mulesoft.com/pdk/latest/policies-pdk-architecture

- PDK Policy Templates
  - https://docs.mulesoft.com/pdk/latest/policies-pdk-policy-templates

- MuleSoft MCP Support
  - https://www.mulesoft.com/platform/ai/modelcontextprotocol

