# Write a connector

A connector is how a run reaches a company system, and the gateway is the only
place credentials live. You have three options, in order of effort.

## Option 1: configure a built-in connector

The gateway ships with `sqlite` (named statements), `http` (host-allow-listed
requests), `fs` (files under a root) and `mcp` (any stdio MCP server). Most
systems with an HTTP API or a database need no code:

```json
{
  "name": "crm",
  "type": "http",
  "allowed_hosts": ["crm.internal.example"],
  "tools": {
    "get": {"writes": false, "contract": {"required": {"status": "number", "body": "string"}}},
    "post": {"writes": true, "contract": {"required": {"status": "number"}}}
  },
  "probe": {"tool": "get", "args": {"url": "https://crm.internal.example/health",
                                     "headers": {"Authorization": "Bearer ${CRM_TOKEN}"}}}
}
```

`${CRM_TOKEN}` is substituted from the gateway's environment at load time and
never appears in a log, an event or a response. The scope of every call is
derived from the URL's host, so a remit lists `http:host:crm.internal.example`.

## Option 2: point at an MCP server

```json
{"name": "docs", "type": "mcp", "command": ["npx", "-y", "some-mcp-server"],
 "env": {"DOCS_TOKEN": "${DOCS_TOKEN}"}}
```

The gateway starts the server, lists its tools as `docs.<tool>`, takes `writes`
from the server's tool annotations, and requires the literal scope `mcp:docs:*`
in the remit unless the server declares a scope of its own.

## Option 3: write one in Go

For a system with its own protocol, or where scope must be derived from the
arguments in a way the built-ins cannot, implement the `connect.Connector`
interface from `github.com/rhs2/kernos/gateway/connect`:

```go
package ledger

import (
    "context"
    "github.com/rhs2/kernos/gateway/connect"
)

type Ledger struct{ client *api.Client }

func New(cfg map[string]any) (connect.Connector, error) {
    return &Ledger{client: api.Dial(cfg["endpoint"].(string), cfg["token"].(string))}, nil
}

func (l *Ledger) Name() string { return "ledger" }

func (l *Ledger) Tools() []connect.ToolSpec {
    return []connect.ToolSpec{{
        ID: "ledger.post_entry", Description: "Post a journal entry", Writes: true,
        InputSchema: map[string]any{"type": "object", "required": []any{"period", "lines"}},
        Contract: connect.Contract{Required: map[string]string{"entry_id": "string"}},
        ScopeDerivation: "period",
    }}
}

func (l *Ledger) Call(ctx context.Context, tool string, args map[string]any) (map[string]any, []string, error) {
    period, _ := args["period"].(string)
    scopes := []string{"ledger:period:" + period}
    entry, err := l.client.Post(ctx, args)
    if err != nil {
        if api.IsValidation(err) {
            return nil, scopes, fmt.Errorf("%w: %v", connect.ErrDeterministic, err) // do not retry
        }
        return nil, scopes, err // retried by the kernel's policy
    }
    return map[string]any{"entry_id": entry.ID}, scopes, nil
}

func (l *Ledger) Probe(ctx context.Context) (map[string]any, error) {
    return l.client.Health(ctx) // harmless, checked against the contract every interval
}

func init() { connect.Register(New, "ledger") }
```

Three rules make a connector trustworthy:

1. **Derive scope from the arguments**, never from a field the caller supplies.
   The model cannot claim a scope it does not have if the connector computes it.
2. **Wrap deterministic failures** with `connect.ErrDeterministic` so the kernel
   quarantines instead of retrying; leave transient errors unwrapped so they are
   retried with backoff.
3. **Declare a contract and a probe.** The canary loop is what turns an upstream
   schema change into a quarantine and a repair request instead of a month of
   wrong output.

Register the package in your gateway binary (import it for its `init`), rebuild,
and reference it by `type` in `gateway.json`. The full interface is in the
[connector SDK reference](../reference/connector-sdk.md).
