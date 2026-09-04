# Event log reference

Schema version `kernos.events/1`. One append-only stream per run; every step,
model call, tool call, policy decision, approval, retry, budget signal and
error is an event. Run and step state are derived by folding the stream.

## Record

```json
{
  "schema": "kernos.events/1",
  "run_id": "run_01j6zq5v9k3m8x2w4y7a0b1c2d",
  "seq": 7,
  "ts": "2026-09-04T12:00:00.000Z",
  "kind": "tool.called",
  "actor": {"type": "worker", "id": "wrk-a1"},
  "payload": {"step": "post", "tool": "ledger.post_entry", "args": {}, "idempotency_key": "inv-1001"},
  "prev_hash": "0000000000000000000000000000000000000000000000000000000000000000",
  "hash": "9f2c…"
}
```

| Field | Rule |
|---|---|
| `seq` | 1-based, gap-free per run, assigned by the kernel under a transaction |
| `ts` | Kernel clock at append, RFC 3339 UTC with milliseconds; read on replay, never regenerated |
| `actor.type` | `kernel`, `worker`, `gateway`, `policy`, `user`, `system` |
| `prev_hash` | The previous event's `hash`, or 64 zeros for `seq` 1 |
| `hash` | `sha256` over the canonical JSON of `{run_id, seq, ts, kind, actor, payload, prev_hash}` |
| payload size | At most 256 KiB; larger tool results are truncated with `"truncated": true` and a `sha256` of the full content |

Canonical JSON: keys sorted by UTF-16 code units, no insignificant whitespace,
minimal string escapes, integers without exponent, other numbers as the
shortest round-trip form. Only the kernel hashes and signs; every other
component treats hashes and signatures as opaque strings.

## Event kinds

Kinds marked **ext** may be appended by workers and the gateway through the
API; the kernel appends everything else.

| Kind | Payload | Notes |
|---|---|---|
| `run.created` | `bundle_id, bundle_name, bundle_version, workflow, input, remit_id, requested_by, budget {tokens, usd, soft_ratio}` | Always `seq` 1 |
| `step.scheduled` | `step, index, kind` | One per step at creation; again on retry and for compensations |
| `step.leased` | `step, lease_id, worker_id, attempt, expires_at` | |
| `step.lease_expired` | `step, lease_id, worker_id` | The step returns to `scheduled` |
| `step.completed` | `step, lease_id, attempt, output` | |
| `step.failed` | `step, lease_id, attempt, error {code, message}, deterministic` | |
| `step.retry_scheduled` | `step, attempt, delay_ms` | Backoff base 500 ms, factor 2, cap 30 s, jitter; at most 5 attempts |
| `step.quarantined` | `step, reason, attempts` | Three deterministic failures, or five in total |
| `step.escalated` | `step, from_tier, to_tier, reason` | **ext**, low-confidence escalation |
| `step.waiting_approval` | `step, action_id, approval_id` | |
| `model.called` | `step, model, tier, effort, provider, prefix_hash, input_hash, max_tokens` | **ext** |
| `model.responded` | `step, output, usage {input_tokens, output_tokens, cache_read_tokens, cache_write_tokens}, cost_usd, stop_reason, refusal, latency_ms` | **ext** |
| `tool.called` | `step, tool, args, scope, idempotency_key` | **ext**, appended before the gateway call |
| `tool.result` | `step, tool, ok, result, replayed, latency_ms` | **ext** |
| `tool.refused` | `step, tool, reason, remit_id, detail` | **ext**, appended by the gateway |
| `action.proposed` | `action_id, step, action` | |
| `policy.decided` | `action_id, decision, rule, policy, policy_version, approver, sla_seconds, escalate_to` | `allow`, `approval_required` or `deny` |
| `approval.requested` | `approval_id, action_id, approver, sla_seconds, escalate_to, due_at` | |
| `approval.decided` | `approval_id, action_id, decision, actor {id, role}, reason` | `approved` or `rejected` |
| `approval.escalated` | `approval_id, from, to, reason` | SLA expiry |
| `usage.recorded` | `step, tokens, usd, cumulative_tokens, cumulative_usd` | From the usage reported at step completion |
| `budget.soft_threshold` | `cumulative_usd, ceiling_usd, ratio` | Once per run; later leases carry `pacing: true` |
| `budget.exceeded` | `cumulative_usd, ceiling_usd` | The run parks |
| `run.parked` | `reason, detail` | `approval`, `budget`, `quarantine`, `connector_quarantined`, `refusal`, `human` |
| `run.resumed` | `reason` | |
| `run.abandoned` | `reason, actor` | Followed by compensation events |
| `compensation.scheduled` | `step, for_step, tool, args` | Reverse order of completed steps |
| `compensation.completed` | `step, for_step, result` | |
| `compensation.failed` | `step, for_step, error` | The run ends `failed` with `needs_human` |
| `run.completed` | `output` | |
| `run.failed` | `error {code, message}, needs_human` | |
| `note` | `text, data` | **ext**, diagnostics; never affects state |

External appends are checked: the poster must hold the step's live lease
(`X-Kernos-Lease`) or present a remit token valid for the run
(`X-Kernos-Remit`), and only **ext** kinds are accepted.

## State machines

Run: `created` → `running` → (`parked` ↔ `running`) → `completed` | `failed` | `abandoned`.

Step: `scheduled` → `leased` → `completed` | `failed` → (`scheduled` on retry) | `quarantined`
| `waiting_approval` → `scheduled` (approved) | `failed` (rejected).

Steps run in bundle order. Compensation steps are scheduled after
`run.abandoned` in reverse order of the steps they compensate.

## Fold and replay

`fold(events)` is a pure function producing the run state (run and step
states, attempts, outputs, budget totals, pending approval, decisions,
compensations). The kernel's materialised tables are written in the same
transaction as each event and equal the fold at every sequence number.

`replay` verifies three things: the hash chain (every `prev_hash` and `hash`),
the state (fold equals materialised), and the decisions (every `policy.decided`
re-evaluated with the recorded policy version against the recorded action and
run context). Non-deterministic values are read from the log, never
regenerated.

## Idempotency of external writes

A worker appends `tool.called` with an idempotency key before calling the
gateway. After a lost lease the worker reuses a recorded `tool.result` with the
same key, or re-sends a call that has no result with the same key, and the
gateway's idempotency store returns the earlier result if the first call did
complete. An external write happens at most once per key.
