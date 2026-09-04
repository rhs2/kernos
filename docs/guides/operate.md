# Operate

What an operator does in a normal week, and what the system does on its own.

## Approvals

```bash
kernos approvals list --state pending
kernos approvals decide apr_… --approve --as u-tom --role finance_admin --reason "Matches PO 4471"
kernos approvals decide apr_… --reject  --as u-tom --role finance_admin --reason "Duplicate of inv-0998"
```

A decision needs a reason. The actor must match the approver the policy named:
a role approver accepts any actor with that role, a user approver accepts that
user only. An unanswered request escalates up the reporting line when its SLA
passes, and parks the run for a human after the second expiry.

## Parked runs

```bash
kernos run list --state parked
kernos run show run_…
```

The run's `parked` reason says what to do:

| Reason | What happened | What to do |
|---|---|---|
| `approval` | Waiting for a decision | Decide it |
| `budget` | Hit its spend ceiling | Issue a wider remit and restart, or accept the partial result |
| `quarantine` | A step failed deterministically three times | Read the step's error in the log; fix the input or the bundle |
| `connector_quarantined` | A canary failed on a connector it needs | Fix the mapping, release the connector, resume the run |
| `refusal` | The model refused | Read the prompt and answer in the log; adjust the bundle or route to a human |
| `human` | An escalation expired twice, or an operator parked it | Decide, then `kernos run resume` |

## Canaries and repairs

```bash
curl -s http://127.0.0.1:7402/v1/canaries | jq
ls gateway-data/repairs/
```

A quarantined connector has a repair request on disk with the contract, the
observed shape and the diff. Fix the statement or mapping in `gateway.json`,
reload the gateway, probe once (`POST /v1/canaries/<connector>/probe`), then
release it (`POST /v1/canaries/<connector>/release`). Runs that parked on it
resume with `kernos run resume`.

## Abandoning a run

```bash
kernos run abandon run_… --reason "customer cancelled"
```

The kernel schedules the compensation of every completed step that declared
one, in reverse order, and records each result. A compensation that fails after
retries leaves the run `failed` with `needs_human` set and everything it did in
the log.

## Reading a log

```bash
kernos run events run_… | less
kernos run replay run_…
```

Every model call carries the prompt hash and the answer, every tool call its
arguments and result, every decision its rule and policy version, every
approval its actor and reason. Replay verifies the chain and reproduces the
decisions; a mismatch names the sequence number.

## Changing things safely

| Change | How |
|---|---|
| A workflow | New bundle version, signed, applied; score it first with `kernos-eval` |
| A gate | New policy version; run `kernos policy test` against recent actions first |
| A connector mapping | Edit `gateway.json`, reload, probe, release |
| A model | Point candidate workers at it, score, promote through the gate |
| A remit | Issue a new one; remits are never edited |

## Metrics worth alerting on

`kernos_gateway_refusals_total` rising (a run keeps trying to leave its remit),
`kernos_gateway_canary_status` at -1 (a connector is quarantined),
`kernos_approvals_pending` older than the SLA, `kernos_runs{state="parked"}`
growing, and `kernos_leases_expired_total` rising (workers are dying mid-step).
