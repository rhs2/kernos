# Kernel API reference

The `kernos` binary serves the kernel and the control plane on one listener,
default `http://127.0.0.1:7401`, base path `/v1`. Bodies are JSON. If
`KERNOS_TOKEN` is set, every request carries `Authorization: Bearer <token>`;
otherwise the listener is loopback-only.

Errors: `{"error": {"code": "snake_case_code", "message": "...", "details": {}}}`
with a matching status. Codes are stable.

## Health and keys

`GET /v1/health` → `{"ok", "version", "uptime_s", "runs": {"running", "parked"}}`

`GET /v1/keys` → `{"key_id", "algorithm": "ed25519", "public_key"}` (base64url, 32 bytes)

## Bundles

`POST /v1/bundles` `{"bundle": {…}, "signature": {"key_id", "signature"}}` → `201 {"bundle_id", "name", "version"}`

The signature is verified over the canonical JSON of `bundle` against the
trusted publisher keys. Unsigned or unknown-key bundles: `422 bundle_signature_invalid`.
Validation failures: `422 bundle_invalid` with `details.path`. Re-posting the
same name and version with identical content returns `200`; different content
is `409 bundle_version_exists`.

`GET /v1/bundles` · `GET /v1/bundles/{id}`

## Policies

`POST /v1/policies` `{"name", "version", "source"}` → `201 {"policy_id", "name", "version"}`;
parse errors `422 policy_invalid` with `details.line`, `details.column`, `details.message`.

`GET /v1/policies` · `GET /v1/policies/{name}` · `GET /v1/policies/{name}/{version}`

`POST /v1/policies/test` `{"policy_a": {name, version}, "policy_b": {name, version} | {"source"}, "corpus": [{action, run}]}`
→ `{"cases", "flips": [{"index", "a", "b", "rule_a", "rule_b"}]}`

## Remits

`POST /v1/remits`

```json
{"tools": ["ledger.*"], "scopes": ["sql:table:ledger_entries"], "grants": [],
 "spend": {"tokens": 200000, "usd": 2.0}, "autonomy": "supervised", "ttl_seconds": 86400,
 "policy_set": ["finance-default"], "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}}
```

→ `201 {"remit_id", "token", "expires_at"}`

`POST /v1/remits/{id}/derive` with any subset of the fields; every given field
must narrow or the answer is `422 remit_widens` with `details.field`.
→ `201 {"remit_id", "parent_id", "token", "expires_at"}`

`GET /v1/remits/{id}` → the payload, `parent_id`, `run_id`.

## Runs

`POST /v1/runs` `{"bundle_id", "workflow", "input", "remit_id", "requested_by"}` → `201 {"run_id", "state"}`.
Input is validated against the workflow schema (`422 input_invalid`); a remit
already bound to a run is `409 remit_bound`.

`GET /v1/runs?state=&department=&limit=&after=` → `{"runs", "next"}`
`GET /v1/runs/{id}` → the run state
`GET /v1/runs/{id}/events?from_seq=&limit=` → `{"events", "next_seq"}`
`POST /v1/runs/{id}/events` `{"kind", "payload", "actor"}` → `201 {"seq", "hash"}` (external kinds only; `X-Kernos-Lease` or `X-Kernos-Remit` required)
`POST /v1/runs/{id}/replay` → `{"chain_valid", "events", "state_matches", "decisions", "decision_mismatches", "chain_errors", "state"}`
`POST /v1/runs/{id}/abandon` `{"reason", "actor"}` → `202 {"compensations_scheduled"}`
`POST /v1/runs/{id}/resume` `{"actor"}` → resumes a run parked for `human`, `quarantine` or `connector_quarantined`

## Leases

`POST /v1/leases` `{"worker_id", "kinds": ["model", "tool", "action", "compensation"], "ttl_seconds"}`
→ `204` when idle, or `200`:

```json
{"lease_id", "run_id", "step", "attempt", "expires_at", "heartbeat_seconds",
 "step_def": {…},
 "context": {"input", "steps": {"<id>": {"output"}}, "run": {"id", "bundle", "workflow", "requested_by", "department"},
             "remit_token", "remit": {"autonomy", "grants", "tools", "scopes"},
             "prompts", "mock", "tools", "pacing", "approved_actions", "prior_events"}}
```

`POST /v1/leases/{id}/heartbeat` → `{"expires_at"}` or `410 lease_expired`
`POST /v1/leases/{id}/complete` `{"output", "usage": {"tokens", "usd"}}` → `{"run_state", "next_step"}`
`POST /v1/leases/{id}/fail` `{"error": {"code", "message"}, "deterministic"}` → `{"outcome": "retry_scheduled" | "quarantined", "delay_ms"}`
`POST /v1/leases/{id}/actions` `{"action": {…}}` → `{"action_id", "decision", "rule", "approval_id"}`

On `approval_required` the kernel parks the run and releases the lease; the
worker stops the step at once. After approval the step is re-leased with
`approved_actions` containing the id, and the same proposal returns `allow`
with rule `approved:<approval_id>`. On `deny` the response is `403 action_denied`.

## Approvals

`GET /v1/approvals?state=pending&approver=role:finance_admin`
`POST /v1/approvals/{id}` `{"decision": "approved" | "rejected", "actor": {"id", "role"}, "reason"}` → `{"run_id", "run_state"}`.
The actor must match the approver (`403 not_the_approver`); deciding twice is `409 already_decided`.

## Metrics

`GET /v1/metrics` in Prometheus text: `kernos_runs{state}`, `kernos_steps_leased_total`,
`kernos_step_latency_seconds`, `kernos_usage_usd_total{department}`,
`kernos_approvals_pending`, `kernos_leases_expired_total`, `kernos_events_total{kind}`.
