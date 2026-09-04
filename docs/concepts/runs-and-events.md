# Runs and the event log

Every run is a durable, replayable log. State is an append-only stream of
events; the run survives a restart, resumes from its last committed step, and
replays deterministically to reproduce any decision.

## One stream per run

Every step, model call, tool call, policy decision, approval, retry, budget
signal and error is an event with a sequence number, a timestamp, an actor and
a payload. Each event carries the hash of the previous one, so the stream is a
chain: a later bug, or a later person, cannot rewrite history without breaking
it. The kernel is the only component that writes durable state.

```
seq  kind                  actor     payload
1    run.created           kernel    bundle, workflow, input, remit, budget
2    step.scheduled        kernel    extract
3    step.leased           kernel    extract, worker wrk-a1
4    model.called          worker    claude-sonnet-5, effort low, prefix hash
5    model.responded       worker    output, usage, cost
6    step.completed        kernel    extract
...
11   action.proposed       kernel    payment.issue 7250 USD
12   policy.decided        policy    approval_required, finance-default@1#0
13   approval.requested    kernel    role finance_admin, due in 4h
14   run.parked            kernel    approval
15   approval.decided      user      u-tom, "matches PO 4471"
16   run.resumed           kernel
```

## Leases, not locks

A worker takes a lease on a step and heartbeats while it works. If the worker
dies, the lease expires and another worker resumes from the last committed
event. Steps are idempotent by construction: a step that writes to an external
system records an idempotency key in the log before the call, so a resumed
step detects that the write already happened rather than repeating it, and the
gateway's own idempotency store catches the case where the first call completed
but its result was never recorded.

## Retries, poison, budgets

A failure is either deterministic (a schema violation, a refused call, a denied
action) or not (a timeout, an upstream error). Non-deterministic failures retry
with exponential backoff and jitter; deterministic ones retry a few times and
then the step is quarantined with its log attached and the run parks. Nothing
is retried forever.

Every run declares a budget in tokens and currency. Approaching the ceiling
sets a pacing flag that workers honour; crossing it parks the run. Work is
never silently truncated.

## Compensation

A step that writes to a system of record may declare a compensating action.
When a run is abandoned, the kernel walks the log backwards and schedules the
compensations of every completed step, in reverse order, and records each
result. This is sagas rather than distributed transactions, and it is the only
honest way to unwind work spread across systems that do not share a
transaction manager.

## Replay

Replaying a run recomputes the hash chain, folds the events back into a run
state and compares it with what the kernel holds, and re-evaluates every policy
decision against the recorded policy version and the recorded action.
Non-deterministic values (model outputs, tool results, timestamps) are read
from the log, never regenerated. Two consequences: a production incident is
debuggable months later, and an upgraded engine can be checked against real
historical runs before it is promoted.

See the [event log reference](../reference/events.md) for every event kind and
the state machines.
