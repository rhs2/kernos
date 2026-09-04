# Security

## The guarantees

- **A run cannot exceed its remit.** Authority is a signed capability verified
  at the gateway on every call. Prompt content, and a model persuaded by it,
  cannot change what the gateway allows. Refusals are recorded events.
- **Delegation narrows.** A child remit is always a subset of its parent. There
  is no path by which fan-out escalates privilege.
- **The log cannot be rewritten silently.** Events are hash-chained and
  append-only; replay recomputes the chain and reports the first broken link.
- **Secrets never reach the reasoning layer.** Connectors hold credentials; the
  gateway substitutes them at egress from its own environment and never
  serialises them into a response, a log line or an event. A prompt cannot leak
  a key it was never given.
- **Approvals are records**, with actor, reason and timestamp, produced by a
  versioned policy that is tested against historical actions before it is
  applied.
- **Bundles are signed.** The control plane refuses unsigned bundles and
  unknown publisher keys.
- **Automatic repair may touch a mapping, never a policy, a remit or a
  compensation.**

## The data boundary

Before content reaches a model provider it passes a boundary that redacts
according to the remit's data grants. What was redacted, and how many fields,
is recorded on the run. This is what lets a company answer "did customer data
leave our systems, and when" with evidence rather than assurance.

## Prompt injection is contained, not prevented

Content fetched from the world is untrusted and is treated as such. The defence
is structural: a persuaded model still cannot exceed its remit. It can be
talked into trying, the gateway refuses, and the refusal is alertable. Systems
that rely on the model not being fooled are one clever document away from an
incident.

## Least privilege by construction

Three roles for the datastore, per-connector credentials scoped to the
narrowest grant the upstream supports, remits that expire, and a loopback-only
default for every listener until a token is configured.

## Supply chain

Dependency auditing in CI for every language, container images built from
pinned bases and run as non-root, full-history secret scanning on every push,
signed bundles, and pinned model versions that only move through the promotion
gate.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on the repository. Details and
response times are in [SECURITY.md](https://github.com/rhs2/kernos/blob/main/SECURITY.md).
