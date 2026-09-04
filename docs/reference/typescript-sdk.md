# TypeScript SDK reference

Package `@kernos/sdk`. Zero runtime dependencies, ESM and CommonJS, type
declarations included, Node 18+ or any browser with `fetch`.

```bash
npm install @kernos/sdk
```

## Usage

```ts
import { KernosClient } from "@kernos/sdk";

const k = new KernosClient({ baseUrl: "http://127.0.0.1:7401", token: process.env.KERNOS_TOKEN });

await k.health();
await k.keys();
await k.bundles.apply(bundle, signature);
await k.bundles.list();
await k.bundles.get(id);
await k.policies.apply({ name, version, source });
await k.policies.test({ policy_a, policy_b, corpus });
await k.remits.issue({ tools, scopes, grants, spend, autonomy, ttl_seconds, policy_set, requested_by });
await k.remits.derive(remitId, { tools });
await k.runs.start({ bundle_id, workflow, input, remit_id, requested_by });
await k.runs.get(runId);
await k.runs.list({ state: "parked" });
await k.runs.events(runId, { from_seq: 1 });
for await (const event of k.runs.follow(runId)) { /* until the run ends */ }
await k.runs.replay(runId);
await k.runs.abandon(runId, { reason, actor });
await k.approvals.list({ state: "pending" });
await k.approvals.decide(approvalId, { decision: "approved", actor, reason });
```

## Errors

Non-2xx answers throw `KernosError` with `status`, `code`, `message` and
`details`; transport failures throw `KernosNetworkError`.

## Types

`RunState`, `Event`, `EventKind`, `Remit`, `Approval`, `Bundle`,
`PolicyDecision` and every request and response body are exported and mirror
the [kernel API](kernel-api.md).

`runs.follow` polls events with `from_seq` every `pollMs` (default 500) and
completes on `run.completed`, `run.failed` or `run.abandoned`.
