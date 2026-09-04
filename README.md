<p align="center">
  <img src="docs/assets/kernos-mark.svg" width="72" alt="Kernos">
</p>

<h1 align="center">Kernos</h1>

<p align="center"><strong>One runtime, every department. Agents that work inside their remit, stop at the gates you set, and keep running for months without anyone editing the code.</strong></p>

<p align="center">
  <a href="https://github.com/rhs2/kernos/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/rhs2/kernos/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://crates.io/crates/kernos"><img alt="crates.io" src="https://img.shields.io/crates/v/kernos.svg"></a>
  <a href="https://pypi.org/project/kernos-sdk/"><img alt="PyPI" src="https://img.shields.io/pypi/v/kernos-sdk.svg"></a>
  <a href="https://www.npmjs.com/package/@kernos/sdk"><img alt="npm" src="https://img.shields.io/npm/v/%40kernos%2Fsdk.svg"></a>
  <a href="https://pkg.go.dev/github.com/rhs2/kernos/gateway"><img alt="Go reference" src="https://pkg.go.dev/badge/github.com/rhs2/kernos/gateway.svg"></a>
  <a href="LICENSE"><img alt="Apache 2.0" src="https://img.shields.io/badge/license-Apache%202.0-blue.svg"></a>
</p>

<p align="center"><a href="https://rhs2.github.io/kernos/">Documentation</a> · <a href="https://rhs2.github.io/kernos/getting-started/quickstart/">Quickstart</a> · <a href="https://rhs2.github.io/kernos/reference/kernel-api/">API reference</a> · <a href="https://github.com/rhs2/kernos/releases/latest">Releases</a> · <a href="CHANGELOG.md">Changelog</a></p>

---

A *kernos* is a Greek ritual vessel: a single ring with many small cups attached
to it, each holding a different offering, all carried as one object. That is the
shape of this system. Finance, people, revenue, support, operations, legal and
engineering each get their own cup. The ring is shared.

Kernos is an open engine for company-wide automation with language models. It is
deliberately **empty of business logic**: the runtime, the capability model, the
policy engine, the connector protocol and the evaluation harness are here, while
the workflows, prompts and rules that describe a particular company stay private
to that company and load as signed bundles.

## The three problems it exists to solve

**Authority is all or nothing.** An agent is handed an API key and inherits
everything that key can reach. Kernos issues a signed **remit** instead: the exact
tools, the exact data scopes, the exact spend, and an expiry. A call outside it is
refused at the gateway, not by the model's good manners.

**Approval is a message in a chat channel.** Six months later nobody can
reconstruct who allowed what. Kernos makes approval a typed record with an actor,
a reason and a timestamp, produced by a declarative policy rather than a habit.

**Nobody can run it unattended.** Most agent systems need an engineer the week an
upstream API changes. Kernos is built so that the normal response to change is
automatic: contract canaries catch drift before a real run does, every run is a
replayable log that survives restarts, and every change to a prompt, model,
connector or bundle ships through the same evaluate, promote-or-rollback gate.

## What is in the box

| Package | Install | What it is |
|---|---|---|
| [`kernos`](https://crates.io/crates/kernos) | `cargo install kernos` | The kernel and control plane: durable hash-chained event log, deterministic replay, scheduler with leases, budgets, compensation, remits, policy, approvals, and the operator CLI |
| [`kernos-core`](https://crates.io/crates/kernos-core) · [`kernos-policy`](https://crates.io/crates/kernos-policy) | `cargo add kernos-core` | The kernel as a library, and the policy language on its own |
| [`kernos/gateway`](https://pkg.go.dev/github.com/rhs2/kernos/gateway) | `go get github.com/rhs2/kernos/gateway` | The only path to company systems: remit verification on every call, scope derivation, idempotency, circuit breakers, contract canaries, connectors for SQL, HTTP, files and MCP servers, and the `connect` SDK for writing your own |
| [`kernos-sdk`](https://pypi.org/project/kernos-sdk/) | `pip install kernos-sdk` | The reasoning worker: model router over three Claude tiers, stable cache prefix, structured outputs, refusal handling, the data boundary, and the evaluation harness with its promotion gate |
| [`@kernos/sdk`](https://www.npmjs.com/package/@kernos/sdk) | `npm install @kernos/sdk` | A typed TypeScript client for the control plane |
| [Images](https://github.com/rhs2/kernos/pkgs/container/kernos-kernel) | `docker pull ghcr.io/rhs2/kernos-kernel` | `kernos-kernel`, `kernos-gateway`, `kernos-worker` |
| [Binaries](https://github.com/rhs2/kernos/releases/latest) | download | Linux and macOS, arm64 and x86_64, with checksums |

Rust for the parts that must never lose a byte, Go for hundreds of flaky
integrations, Python where the model tooling lives, TypeScript for the surface
people touch. Each has one job and a hard interface; the kernel is the only writer
of durable state.

## Sixty seconds

```bash
git clone https://github.com/rhs2/kernos && cd kernos
make build            # Rust stable, Go 1.22+, Python 3.10+, Node 18+
make accept           # the end-to-end suite on the fictional Halcyon Provisions, no API key needed
```

Then walk one invoice through the reference bundle by hand, including the
approval gate and a replay, in the
[quickstart](https://rhs2.github.io/kernos/getting-started/quickstart/).

## What a run looks like

```
seq  kind                  actor     payload
1    run.created           kernel    bundle halcyon.finance.invoice_intake@1.0.0, remit rem_…, budget 2.00 USD
3    step.leased           kernel    extract, worker wrk-a1
4    model.called          worker    claude-sonnet-5, effort low, prefix 9f2c…
5    model.responded       worker    vendor Northwind Dairy, total 7250.00, usage 412 tokens
11   action.proposed       kernel    payment.issue 7250.00 USD, writes to ledger
12   policy.decided        policy    approval_required, finance-default@1#0
13   approval.requested    kernel    role finance_admin, due in 4h
14   run.parked            kernel    approval
15   approval.decided      user      u-tom, "matches PO 4471"
16   run.resumed           kernel
19   tool.called           worker    ledger.post_entry, idempotency inv-1001
20   tool.result           worker    entry 88, posted
22   run.completed         kernel
```

Every line is an event in an append-only, hash-chained log. Kill the worker at
seq 19 and another finishes the run without posting twice. Replay the log and
every decision comes back identical. Abandon the run and the entry is voided by
the compensation the bundle declared.

## Guarantees

- A run cannot call a tool, touch a scope, or spend past what its remit names.
  Delegation to sub-runs can only narrow.
- Approvals are records with actor, reason and timestamp, decided by a versioned
  policy that is tested against historical actions before it is applied.
- Runs survive restarts, resume from the last committed event, never repeat an
  external write, and unwind completed writes when abandoned.
- Upstream drift is caught by contract canaries before a real run sees it; the
  connector is quarantined and a repair request is filed, so the system never
  guesses.
- Secrets live in the gateway and never reach a prompt, a log or an event.

## Status

`0.1.0` is the first release. It ships the kernel, gateway, reasoning worker,
TypeScript client, one reference bundle and an acceptance suite of thirteen
end-to-end scenarios that runs in CI and before every tag. Not yet included: the
console, live-traffic shadow runs in the promotion gate, automated repair
proposals, and the Helm chart. See the [changelog](CHANGELOG.md).

## Contributing and licence

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request, and
[SECURITY.md](SECURITY.md) for how to report a vulnerability privately.

Kernos is released under the [Apache License 2.0](LICENSE). Run it across your
whole company, modify it, build commercial products on it, keep every change
private. Your bundles are your own work and this licence does not reach them.
The reasoning is in [LICENSE.md](LICENSE.md).
