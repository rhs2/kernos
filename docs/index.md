---
hide:
  - navigation
---

<div class="kn-hero" markdown>

# Kernos

<p class="kn-lead">One runtime, every department. Agents that work inside their remit, stop at the gates you set, and keep running for months without anyone editing the code.</p>

<span class="kn-pill">Apache 2.0</span>
<span class="kn-pill">Rust · Go · Python · TypeScript</span>
<span class="kn-pill">v0.1.0</span>

</div>

A *kernos* is a Greek ritual vessel: a single ring with many small cups attached
to it, each holding a different offering, all carried as one object. Finance,
people, revenue, support, operations, legal and engineering each get their own
cup. The ring is shared.

Kernos is an open engine for company-wide automation with language models. It is
deliberately empty of business logic: the runtime, the capability model, the
policy engine, the connector protocol and the evaluation harness are here, while
the workflows, prompts and rules that describe a particular company stay private
to that company and load as signed bundles.

<div class="kn-grid" markdown>

<div class="kn-card" markdown>
### Remits
A run carries a signed capability set: the exact tools, data scopes, spend and
expiry. The gateway refuses anything outside it, whatever the model was
persuaded to try. [Read more](concepts/remits.md)
</div>

<div class="kn-card" markdown>
### Policy and approvals
Which actions need a human is a declarative rule. Approval is a typed record
with actor, reason and timestamp, reconstructible months later.
[Read more](concepts/policy.md)
</div>

<div class="kn-card" markdown>
### Durable, replayable runs
Every run is an append-only, hash-chained event log. It survives restarts,
never repeats an external write, and replays to reproduce every decision.
[Read more](concepts/runs-and-events.md)
</div>

<div class="kn-card" markdown>
### Running unattended
Contract canaries catch upstream drift before a real run does; every change
ships through one evaluate and promote-or-rollback gate.
[Read more](concepts/unattended.md)
</div>

</div>

## Start here

- [Quickstart](getting-started/quickstart.md): build, run the acceptance suite,
  then walk one invoice through the reference bundle, approve it, and replay it.
- [Architecture](concepts/architecture.md): the four components, why each is in
  the language it is, and what talks to what.
- [Write a bundle](guides/write-a-bundle.md): describe a workflow as data and
  load it without a deploy.
- [Kernel API](reference/kernel-api.md), [Gateway API](reference/gateway-api.md),
  [CLI](reference/cli.md): the complete reference.

## Packages

| Package | Where | What |
|---|---|---|
| `kernos`, `kernos-core`, `kernos-policy` | crates.io | Kernel, control plane, CLI, policy language |
| `github.com/rhs2/kernos/gateway` | Go module | Gateway binary and the `connect` SDK |
| `kernos-sdk` | PyPI | Reasoning worker and evaluation harness |
| `@kernos/sdk` | npm | TypeScript client |
| `ghcr.io/rhs2/kernos-{kernel,gateway,worker}` | GHCR | Container images |

The source is at [github.com/rhs2/kernos](https://github.com/rhs2/kernos).
