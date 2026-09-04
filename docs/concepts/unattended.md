# Running unattended

The requirement most agent platforms fail: the system runs for months, absorbs
upstream change, and only involves a human when it genuinely cannot proceed.
Five mechanisms carry it.

## 1. Behaviour is data, hot-swapped

Bundles are signed archives of step definitions, prompt templates, policy rules
and tool bindings, loaded and verified at runtime. Changing what a workflow does
is a bundle publish: no container rebuild, no restart, no engineer. In-flight
runs finish on the version they started with.

## 2. Contract canaries catch drift before a real run does

Every connector declares the shape it expects from upstream, and a harmless
probe that exercises it. The gateway runs the probes continuously and compares
each response to the contract. When a field disappears, changes type, or a
new required parameter appears, the canary fails before a production run hits
it. The connector is quarantined, runs that need it park rather than producing
wrong output, and a repair request is filed with the contract, the observed
shape and the diff.

This inverts the normal failure mode. Instead of discovering a schema change
because a month of invoices were coded wrong, the system knows within minutes
and refuses to guess.

## 3. Repair, then escalation

A repair request is the ticket the system files against itself: everything a
human needs to fix a mapping in twenty minutes instead of two days of
forensics. Automated repair proposals, which read the upstream's current schema
and submit a candidate mapping through the promotion gate, are on the roadmap.
The rule is fixed already: an automatic repair may fix a mapping, never a
policy, never a remit, never a compensation. The system may adapt to how an
upstream speaks; it may never widen what it is allowed to do.

## 4. One promotion gate for every kind of change

Prompts, models, connectors, bundles and the engine itself ship through the
same pipeline:

```
candidate -> evaluation against the golden set
          -> cost and latency comparison
          -> promote, or roll back on regression
```

The evaluation harness runs every case as a real run through the kernel, so
the log is the evidence, and the gate refuses a candidate that lowers the pass
rate or raises cost past a threshold. This is what makes model upgrades safe
without a human: a new model is a candidate, evaluated, and promoted only if it
is better on quality and acceptable on cost.

## 5. The system tends itself

- Circuit breakers per connector, with backoff and jitter; a failing upstream
  degrades one lane, not the platform.
- Poison-input quarantine: a step that fails deterministically three times is
  parked with its log rather than retried forever.
- Budget guards: throttle first, park second, never silently truncate.
- Nightly reconciliation against the systems of record, and golden-set drift
  detection, are the reference bundles' job and ship as examples.

## What still needs a human, by design

A policy change, a remit widening, a compensation for an unwound run, and any
repair the gate rejected. Those are the decisions that should never be
automatic, and keeping the list short and explicit is the point.
