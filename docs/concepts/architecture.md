# Architecture

```
                            +-----------------------------+
   Console and clients ---->|  Control plane (Rust)       |
   approvals, run           |  remits, policy, registry,  |
   inspector, cost          |  approvals, audit           |
                            +--------------+--------------+
                                           |
                            +--------------v--------------+
                            |  Kernel (Rust)              |
                            |  durable log, scheduler,    |
                            |  replay, leases, budgets    |
                            +---+---------------------+---+
                                |                     |
             +------------------v----+       +--------v-------------------+
             | Reasoning (Python)    |       | Gateway (Go)               |
             | step runner, model    |       | MCP host, connector fleet, |
             | router, evaluation    |       | remit enforcement, canaries|
             +-----------------------+       +----------------------------+
                                |                     |
                          Claude models          Company systems
                                                 (ERP, CRM, HRIS, ledger,
                                                  mail, docs, source control)
```

The kernel is the only component that writes durable state. Workers are
disposable: they lease a step, do it, and report back. The gateway is
stateless apart from its idempotency store and canary status. The control
plane shares a process and a database with the kernel and serves the same API.

## Why each language

Four languages needs justifying. Each is here because the alternative is worse.

**Rust for the kernel and control plane.** The kernel owns the durable log,
deterministic replay, the scheduler and budget accounting, and it runs for
weeks under mixed load. A pause or a memory error here corrupts the record every
other guarantee depends on. Rust gives predictable latency with no garbage
collection in the hot path, and an ownership model that lets the replay
invariants be enforced at compile time rather than in review.

**Go for the gateway and connectors.** The integration surface is hundreds of
concurrent conversations with systems that are slow and flaky in different
ways. Goroutines model that directly; connectors compile to single static
binaries that are trivial to deploy and sandbox; and connectors are the part
most often written by someone outside the core team, and Go is the easiest of
these four to get right on a first attempt.

**Python for reasoning and evaluation.** The model SDKs, evaluation libraries,
dataset tooling and notebooks live here. The layer is deliberately thin: compose
a turn, call the model, hand tool calls to the gateway. It holds no durable
state, so workers are disposable.

**TypeScript for the surface people touch.** The approvals inbox and the run
inspector are what non-technical staff see, so the client has to be good.

## What talks to what

| From | To | Over | Carrying |
|---|---|---|---|
| Worker | Kernel | HTTP `/v1/leases`, `/v1/runs/{id}/events` | leases, heartbeats, completions, model and tool events, action proposals |
| Worker | Gateway | HTTP `/v1/tools/call` | the run-bound remit token, the tool, its arguments, an idempotency key |
| Gateway | Kernel | HTTP `/v1/keys`, `/v1/runs/{id}/events` | the public key for remit verification; refusal events |
| Console, CLI, clients | Kernel | HTTP `/v1/*` | bundles, policies, remits, runs, approvals, replay |
| Gateway | Company systems | connector-specific | the actual work, with credentials the worker never saw |

Every listener is loopback-only until a bearer token is configured. Nothing in
the reasoning layer ever holds a credential for a company system.

## One log, many readers

The event log is the integration point for everything else: the console reads
it directly, replay folds it, metrics are derived from it, approvals are
reconstructed from it, and the evaluation harness scores real runs through it.
What an operator sees is exactly what happened, not a summary of it.
