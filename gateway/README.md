# kernos-gateway

The Go gateway of [Kernos](../README.md): the only path from a run to a
company system. It hosts connectors, verifies the signed remit on every tool
call, holds credentials so the reasoning layer never sees them, keeps an
idempotency store, runs contract canaries and exposes Prometheus metrics.

Module `github.com/rhs2/kernos/gateway`. The public connector SDK is the
package `github.com/rhs2/kernos/gateway/connect`. Everything under
`internal/` is private to the binary. The contracts it implements are
[the gateway reference](https://rhs2.github.io/kernos/reference/gateway-api/) and
[the remit reference](https://rhs2.github.io/kernos/reference/remit/); every choice the specs
leave open is recorded in [NOTES.md](NOTES.md).

## Build and run

```
cd gateway
go build -o bin/kernos-gateway ./cmd/kernos-gateway
./bin/kernos-gateway --config gateway.json
```

Go 1.22 or newer. The only third-party dependency is `modernc.org/sqlite`
(pure Go, no cgo), pinned in `go.mod`.

Flags: `--listen`, `--config`, `--data`, `--kernel`, `--log json|text`,
`--version`. Precedence is flags, then environment, then the file.

| Setting | File key | Environment | Default |
|---|---|---|---|
| Listen address | `listen` | `KERNOS_GATEWAY_LISTEN` | `127.0.0.1:7402` |
| Kernel URL | `kernel_url` | `KERNOS_GATEWAY_KERNEL_URL`, `KERNOS_KERNEL_URL` | `http://127.0.0.1:7401` |
| Kernel bearer token | `token` | `KERNOS_GATEWAY_TOKEN`, `KERNOS_TOKEN` | unset |
| Data directory | `data_dir` | `KERNOS_GATEWAY_DATA` | `./gateway-data` |
| Pinned control-plane key | `public_key` | `KERNOS_GATEWAY_PUBLIC_KEY`, `KERNOS_PUBLIC_KEY` | fetched from `/v1/keys` |
| Canary interval (s) | `canary.interval_seconds` | `KERNOS_GATEWAY_CANARY_INTERVAL`, `KERNOS_CANARY_INTERVAL` | 60 |
| Quarantine after N failures | `canary.quarantine_after` | `KERNOS_GATEWAY_CANARY_QUARANTINE_AFTER`, `KERNOS_CANARY_QUARANTINE_AFTER` | 2 |
| Auto release | `canary.auto_release` | `KERNOS_GATEWAY_CANARY_AUTO_RELEASE`, `KERNOS_CANARY_AUTO_RELEASE` | false |
| Test tools | `test_tools` | `KERNOS_GATEWAY_TEST_TOOLS` | off |
| Per-call timeout (s) | `call_timeout_seconds` | `KERNOS_GATEWAY_CALL_TIMEOUT` | 120 |
| Log format and level | `log_format`, `log_level` | `KERNOS_GATEWAY_LOG`, `KERNOS_GATEWAY_LOG_LEVEL` | `json`, `info` |

`${VAR}` inside any string of the file is substituted from the environment
at load, `data_dir` and connector paths included. Substituted credentials are
registered as secrets and redacted from every log line, every response body
and the metrics page (paths and addresses under structural keys such as
`data_dir`, `path` and `command` stay visible; see NOTES.md). A reference to
an unset variable stops the gateway at start.

The `sqlite` connector's optional `"init_sql"` names a `.sql` file (or inline
SQL) that runs once when the database has no tables yet, so a container can
create its schema on first start.

## HTTP API

| Route | Purpose |
|---|---|
| `GET /v1/health` | `{"ok", "version", "connectors": {name: canary status}}` |
| `GET /v1/tools` | every tool with `writes`, `input_schema`, `scope_derivation`, `contract` |
| `POST /v1/tools/call` | the seven remit checks, argument validation, idempotency, quarantine, breaker, then the connector |
| `GET /v1/canaries` | canary status per connector with `contract_diff` |
| `POST /v1/canaries/{connector}/probe` | probe now |
| `POST /v1/canaries/{connector}/release` | clear a quarantine |
| `GET /v1/metrics` | Prometheus text |

Responses of `/v1/tools/call`:

| Status | Body |
|---|---|
| 200 | `{"ok": true, "result", "scope", "replayed", "latency_ms"}` |
| 403 | `{"ok": false, "refusal": {"reason", "detail"}}` and a `tool.refused` event on the run; `observe` and `propose` remits are refused every write tool |
| 404 | `tool_not_found` (the remit allows the tool but no connector exposes it) |
| 409 | `idempotency_conflict` |
| 422 | `args_invalid`, `idempotency_key_required`, `deterministic_failure` (`"deterministic": true`) |
| 502 | `upstream_error` (`"deterministic": false`, `"circuit": "open"` while the breaker is open) |
| 503 | `connector_quarantined` with `connector` and `since` |

## Connectors

Built in: `sqlite`, `http`, `fs`, `mcp`, and `test` (only with
`KERNOS_GATEWAY_TEST_TOOLS=1`). A configuration that exercises them all:

```json
{
  "listen": "127.0.0.1:7402",
  "kernel_url": "http://127.0.0.1:7401",
  "token": null,
  "data_dir": "./gateway-data",
  "canary": {"interval_seconds": 60, "quarantine_after": 2, "auto_release": false},
  "connectors": [
    {"name": "ledger", "type": "sqlite", "path": "./halcyon-ledger.db", "init_sql": "./ledger.sql",
     "tools": {
       "post_entry": {"description": "Post a journal entry", "writes": true,
                      "statement": "insert into ledger_entries(invoice_id, vendor, account, amount, posted_at) values (:invoice_id, :vendor, :account, :amount, :now) returning id as entry_id, posted_at",
                      "input_schema": {"type": "object", "required": ["invoice_id", "vendor", "account", "amount"]},
                      "contract": {"required": {"entry_id": "number", "posted_at": "string"}}},
       "lookup_vendor": {"description": "Find a vendor", "writes": false,
                      "statement": "select id, name, terms from vendors where name = :name",
                      "contract": {"required": {"rows": "list"}}}
     },
     "probe": {"tool": "lookup_vendor", "args": {"name": "__probe__"}}},
    {"name": "http", "type": "http", "allowed_hosts": ["api.halcyon.example", "127.0.0.1"],
     "headers": {"Authorization": "Bearer ${HALCYON_API_TOKEN}"},
     "tools": {"get": {"writes": false, "contract": {"required": {"status": "number", "body": "string"}}}},
     "probe": {"tool": "get", "args": {"url": "http://127.0.0.1:7499/probe"}, "contract": {"required": {"status": "number", "body": "string", "json": "object"}}}},
    {"name": "docs", "type": "fs", "root": "/srv/halcyon/docs"},
    {"name": "crm", "type": "mcp", "command": ["halcyon-crm-mcp"], "env": {"CRM_TOKEN": "${CRM_TOKEN}"}}
  ]
}
```

### Writing a connector

```go
package mine

import (
    "context"

    "github.com/rhs2/kernos/gateway/connect"
)

func init() { connect.Register(New, "mine") }

type conn struct{ name string }

func New(cfg map[string]any) (connect.Connector, error) {
    name, err := connect.ConnectorName(cfg)
    return &conn{name: name}, err
}

func (c *conn) Name() string { return c.name }

func (c *conn) Tools() []connect.ToolSpec {
    return []connect.ToolSpec{{
        ID: connect.ToolID(c.name, "lookup"), Description: "Look something up", Writes: false,
        InputSchema:     map[string]any{"type": "object", "required": []any{"key"}},
        Contract:        connect.Contract{Required: map[string]string{"value": connect.TypeString}},
        ScopeDerivation: connect.ScopeNone,
    }}
}

// Scopes lets the gateway check the remit before Call runs. Without it the
// remit must carry the literal scope "mine:*".
func (c *conn) Scopes(tool string, args map[string]any) ([]string, error) { return nil, nil }

func (c *conn) Call(ctx context.Context, tool string, args map[string]any) (map[string]any, []string, error) {
    key, _ := args["key"].(string)
    if key == "" {
        return nil, nil, connect.Deterministic("key is required")
    }
    return map[string]any{"value": "found " + key}, nil, nil
}

func (c *conn) Probe(ctx context.Context) (map[string]any, error) {
    result, _, err := c.Call(ctx, "lookup", map[string]any{"key": "__probe__"})
    return result, err
}
```

Import the package from a copy of `cmd/kernos-gateway/main.go` and build.
Errors wrapped with `connect.ErrDeterministic` come back as 422 and are not
retried; every other error is a 502 that the kernel's retry policy handles
and that counts towards the connector's circuit breaker.

## Tests

```
go test ./... -count=1
```

The suite is offline: the kernel is an in-process stub, upstreams are
`httptest` servers and the MCP server is the test binary itself.
