# Changelog

All notable changes to Kernos are recorded here. The format follows
Keep a Changelog, versions follow Semantic Versioning. The bundle format
(`kernos.bundle/1`), remit token format (`krt1`) and event schema
(`kernos.events/1`) are versioned independently of the engine and only change
with a migration note in the reference documentation.

## [0.1.0] - 2026-09-04

The first release: the kernel, the gateway, the reasoning worker, the TypeScript client, one reference bundle and the acceptance suite.

### Added

- `kernos-core`: append-only hash-chained event log in SQLite, deterministic
  replay with chain, state and decision verification, scheduler with leases and
  expiry, retries with backoff, poison quarantine, budgets with soft pacing and
  hard park, compensation on abandon, signed remits with delegation narrowing,
  bundle validation and signature verification.
- `kernos-policy`: the declarative policy language, parser with line and column
  errors, total evaluator, approver resolution, escalation, corpus flip testing.
- `kernos`: the kernel and control-plane server with the full `/v1` API,
  Prometheus metrics, structured logs, sweepers, and the operator CLI.
- Gateway (Go): remit verification on every call, scope derivation from
  arguments, idempotency store, circuit breakers, contract canaries with
  quarantine and self-filed repair requests, connectors for SQLite, HTTP, files
  and MCP servers, the `connect` SDK for third-party connectors.
- `kernos-sdk` (Python): the reasoning worker, model router over three Claude
  tiers with adaptive thinking and effort, stable cache prefix, structured
  outputs, refusal handling, confidence escalation, the data boundary, and the
  evaluation harness with the promotion gate.
- `@kernos/sdk` (TypeScript): typed client for the control plane.
- Reference bundle for the fictional Halcyon Provisions: invoice intake with an
  approval threshold, compensation, and a golden set.
- Acceptance suite of thirteen end-to-end scenarios, run in CI and before every tag.
- Release pipeline publishing to crates.io, PyPI, npm, the Go module proxy,
  GHCR and GitHub Releases from one tag.

### Not yet in this release

- The console (approvals inbox, run inspector, policy editor).
- Shadow runs and canary percentages on live traffic in the promotion gate.
- Automated repair proposals for quarantined connectors (repair requests are
  filed; a human applies them).
- Helm chart and the documentation site.
