# Gateway API reference

`kernos-gateway` listens on `127.0.0.1:7402` by default. It verifies the remit
on every call, derives scope from arguments, keeps an idempotency store, runs
circuit breakers and contract canaries, and holds credentials the reasoning
layer never sees.

## Endpoints

`GET /v1/health` → `{"ok", "version", "connectors": {"<name>": "healthy" | "failed" | "quarantined"}}`

`GET /v1/tools` → every tool the connectors expose:

```json
[{"id": "ledger.post_entry", "connector": "ledger", "description": "Post a journal entry",
  "writes": true, "input_schema": {}, "scope_derivation": "table",
  "contract": {"required": {"entry_id": "number", "posted_at": "string"}}}]
```

`POST /v1/tools/call`

```json
{"remit_token": "krt1.…", "run_id": "run_…", "step": "post", "lease_id": "lse_…",
 "tool": "ledger.post_entry", "args": {}, "idempotency_key": "inv-1001", "scope": null}
```

| Status | Body | Meaning |
|---|---|---|
| 200 | `{"ok": true, "result", "scope", "replayed", "latency_ms"}` | Done; `replayed` when the idempotency store answered |
| 403 | `{"ok": false, "refusal": {"reason", "detail"}}` | Outside the remit; a `tool.refused` event is appended to the run |
| 409 | `{"ok": false, "error": {"code": "idempotency_conflict"}}` | Same key, different arguments |
| 422 | `args_invalid` or `deterministic_failure` | Not retried |
| 502 | `upstream_error`, with `"circuit": "open"` when the breaker is open | Retried by the kernel's policy |
| 503 | `connector_quarantined` | A canary quarantined the connector |

Refusal reasons, in verification order: `token_malformed`, `signature_invalid`,
`remit_expired`, `remit_not_yet_valid`, `remit_run_mismatch`, `tool_not_in_remit`,
`scope_not_granted`, `autonomy_too_low`.

`GET /v1/canaries` → per connector: `status`, `last_probe_at`, `last_ok_at`,
`consecutive_failures`, `last_error`, `contract_diff {missing, type_mismatch, unexpected_required}`.
`POST /v1/canaries/{connector}/probe` · `POST /v1/canaries/{connector}/release`

`GET /v1/metrics`: `kernos_gateway_calls_total{tool, outcome}`,
`kernos_gateway_refusals_total{reason}`, `kernos_gateway_call_latency_seconds`,
`kernos_gateway_canary_status{connector}` (1 healthy, 0 failed, -1 quarantined),
`kernos_gateway_circuit_open{connector}`.

## Scope derivation

| Connector | Derivation | Scope string |
|---|---|---|
| `sqlite` | tables in the statement | `sql:table:<name>`, one per table touched |
| `http` | request host | `http:host:<hostname>` |
| `fs` | directory | `fs:path:<absolute directory>` |
| `mcp` | none unless the server declares `x-kernos-scope` | literal `mcp:<server>:*` required |

A call touching several scopes must be granted every one.

## Idempotency store

Keyed by `(tool, idempotency_key)` with the arguments' hash. A repeat with the
same hash returns the stored result with `replayed: true`; a different hash is
`409`. Entries live 30 days in SQLite under `KERNOS_GATEWAY_DATA`.

## Circuit breakers

Per connector: open after 5 consecutive upstream errors, half-open after 10 s,
growing exponentially to 5 min with 20% jitter.

## Contract canaries

Each tool declares the response fields it requires and their JSON types
(`string`, `number`, `bool`, `object`, `list`); each connector declares a
harmless probe. The loop probes every `KERNOS_CANARY_INTERVAL` seconds. After
`KERNOS_CANARY_QUARANTINE_AFTER` consecutive failures the connector is
quarantined, calls return `503`, and a repair request is written to
`KERNOS_GATEWAY_DATA/repairs/<connector>-<timestamp>.json` with the contract,
the observed shape, the diff and the probe. Release is an operator action, or
automatic after two passing probes when `KERNOS_CANARY_AUTO_RELEASE=1`.

## Configuration

```json
{
  "listen": "127.0.0.1:7402",
  "kernel_url": "http://127.0.0.1:7401",
  "token": null,
  "data_dir": "./gateway-data",
  "canary": {"interval_seconds": 60, "quarantine_after": 2, "auto_release": false},
  "connectors": [
    {"name": "ledger", "type": "sqlite", "path": "${HALCYON_LEDGER_DB}",
     "tools": {"post_entry": {"description": "Post a journal entry", "writes": true,
                              "statement": "insert into ledger_entries(...) values (:invoice_id, ...) returning id as entry_id, posted_at",
                              "input_schema": {"type": "object", "required": ["invoice_id", "vendor", "account", "amount"]},
                              "contract": {"required": {"entry_id": "number", "posted_at": "string"}}}},
     "probe": {"tool": "lookup_vendor", "args": {"name": "__probe__"}}},
    {"name": "http", "type": "http", "allowed_hosts": ["api.internal.example"],
     "tools": {"get": {"writes": false, "contract": {"required": {"status": "number", "body": "string"}}}}},
    {"name": "crm", "type": "mcp", "command": ["npx", "-y", "some-mcp-server"], "env": {"CRM_TOKEN": "${CRM_TOKEN}"}}
  ]
}
```

`${VAR}` anywhere in a string is substituted from the gateway's environment at
load; the substituted value never appears in logs, responses or events.

Built-in connector types: `sqlite` (named statements with `:param` and `:now`;
`{"rows": […]}` for selects, the `returning` row for writes), `http` (`get`,
`post`; `allowed_hosts`; returns `status`, `headers`, `body`, `json`), `fs`
(`read`, `list`, `write` under a root), `mcp` (stdio JSON-RPC 2.0 servers;
tools exposed as `<name>.<tool>`). The `test.*` tools exist only with
`KERNOS_GATEWAY_TEST_TOOLS=1`.
