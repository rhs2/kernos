# kernos-core

The [Kernos](https://github.com/rhs2/kernos) kernel as a Rust library: the one
place that writes durable state for company-wide LLM agent automation.

- an append-only, hash-chained event log per run (`kernos.events/1`), with the
  canonical JSON used for every hash and signature implemented exactly once here
- `fold(events) -> RunState`, a pure function the materialised tables must agree with
- a SQLite store (WAL) for events, runs, steps, leases, remits, approvals, bundles
  and policies
- a scheduler with leases, retries with jittered exponential backoff and poison
  quarantine
- budgets with a soft pacing threshold and a hard ceiling
- signed remits (`krt1.` tokens, Ed25519) and delegation that can only narrow
- bundle validation and signature verification
- approval records with SLA escalation, compensation walks on abandon, and
  `replay` that re-verifies the chain, the state and every policy decision

The `kernos` binary is a thin HTTP and CLI layer over this crate.

Licensed under Apache-2.0.
