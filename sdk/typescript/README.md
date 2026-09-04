# @kernos/sdk

Typed TypeScript client for the Kernos kernel and control-plane API
(`https://rhs2.github.io/kernos/reference/kernel-api/`). Zero runtime dependencies: it uses the global
`fetch`, so it works on Node 18 and newer and in browsers. Ships ESM, CommonJS
and `.d.ts` declarations.

## Install

```
npm install @kernos/sdk
```

## Usage

```ts
import { KernosClient, KernosError, KernosNetworkError } from "@kernos/sdk";

const k = new KernosClient({
  baseUrl: "http://127.0.0.1:7401",
  token: process.env.KERNOS_TOKEN, // optional; sent as Authorization: Bearer
});

const health = await k.health();

// Issue a remit and start a run.
const remit = await k.remits.issue({
  tools: ["ledger.*"],
  scopes: ["sql:table:ledger_entries"],
  grants: [],
  spend: { tokens: 200000, usd: 2.0 },
  autonomy: "autonomous",
  ttl_seconds: 3600,
  policy_set: ["finance-default"],
  requested_by: { id: "u-ana", role: "ap_clerk", manager: "u-tom" },
});

const bundles = await k.bundles.list();
const bundle = bundles.find((b) => b.name === "halcyon.finance.invoice_intake");

const run = await k.runs.start({
  bundle_id: bundle!.bundle_id,
  workflow: "intake",
  input: { invoice_id: "inv-1001", text: "...", total: 1250.0 },
  remit_id: remit.remit_id,
  requested_by: { id: "u-ana", role: "ap_clerk", manager: "u-tom" },
});

// Follow the event log until the run ends.
for await (const ev of k.runs.follow(run.run_id)) {
  if (ev.kind === "tool.result") console.log(ev.payload.tool, ev.payload.ok);
  if (ev.kind === "run.parked") console.log("parked:", ev.payload.reason);
}

const state = await k.runs.get(run.run_id); // RunState
const replay = await k.runs.replay(run.run_id); // { chain_valid, state_matches, ... }

// Approvals inbox.
const pending = await k.approvals.list({ state: "pending", approver: "role:finance_admin" });
for (const a of pending) {
  await k.approvals.decide(a.approval_id, {
    decision: "approved",
    actor: { id: "u-tom", role: "finance_admin" },
    reason: "Checked against the delivery note",
  });
}
```

CommonJS works the same way: `const { KernosClient } = require("@kernos/sdk");`.

## API

| Method | Request |
|---|---|
| `health()` | `GET /v1/health` |
| `keys()` | `GET /v1/keys` |
| `metrics()` | `GET /v1/metrics` (Prometheus text) |
| `bundles.apply(bundle, signature)` | `POST /v1/bundles` |
| `bundles.list()`, `bundles.get(id)` | `GET /v1/bundles`, `GET /v1/bundles/{id}` |
| `policies.apply({name, version, source})` | `POST /v1/policies` |
| `policies.test({policy_a, policy_b, corpus})` | `POST /v1/policies/test` |
| `policies.list()`, `policies.get(name)`, `policies.getVersion(name, version)` | `GET /v1/policies...` |
| `remits.issue(req)` | `POST /v1/remits` |
| `remits.derive(remitId, req)` | `POST /v1/remits/{id}/derive` |
| `remits.get(remitId)` | `GET /v1/remits/{id}` |
| `runs.start(req)` | `POST /v1/runs` |
| `runs.get(runId)` | `GET /v1/runs/{id}` |
| `runs.list(query)` | `GET /v1/runs?state=&department=&limit=&after=` |
| `runs.events(runId, {from_seq, limit})` | `GET /v1/runs/{id}/events` |
| `runs.follow(runId, {from_seq, pollMs, limit, signal})` | polls events until `run.completed`, `run.failed` or `run.abandoned` |
| `runs.replay(runId)` | `POST /v1/runs/{id}/replay` |
| `runs.abandon(runId, {reason, actor})` | `POST /v1/runs/{id}/abandon` |
| `runs.resume(runId, {actor})` | `POST /v1/runs/{id}/resume` |
| `runs.appendEvent(runId, {kind, payload, actor}, {lease, remit})` | `POST /v1/runs/{id}/events` with `X-Kernos-Lease` or `X-Kernos-Remit` |
| `leases.acquire(req)` | `POST /v1/leases` (`null` on 204) |
| `leases.heartbeat(id)`, `leases.complete(id, req)`, `leases.fail(id, req)`, `leases.propose(id, req)` | `POST /v1/leases/{id}/...` |
| `approvals.list(query)` | `GET /v1/approvals?state=&approver=` |
| `approvals.decide(approvalId, {decision, actor, reason})` | `POST /v1/approvals/{id}` |

Every request and response body is typed; the types are exported from the
package root and mirror the specification (`RunState`, `Event`, `EventKind`,
`Remit`, `Approval`, `Bundle`, `PolicyDecision`, and the rest). `Event` is a
discriminated union on `kind`, so narrowing on `ev.kind` types `ev.payload`.

## Errors

- `KernosError` for any non-2xx response: `status`, `code` (the kernel's stable
  snake_case code, or `http_<status>` when the body carried no error envelope),
  `message`, `details`, `request`.
- `KernosNetworkError` when no HTTP response arrived (connection refused, DNS,
  abort, unreadable body): `cause` holds the underlying error.
- A 2xx response whose body is empty or not JSON throws `KernosError` with code
  `response_invalid` (except `leases.acquire`, where 204 means no work and resolves to `null`).

## Options

```ts
new KernosClient({
  baseUrl,          // required
  token,            // optional bearer token
  fetch,            // optional fetch replacement (tests, custom agents)
  pollMs,           // default poll interval for runs.follow, default 500
  headers,          // extra headers on every request
});
```

## Build and test

```
npm install
npm run build   # dist/index.js (ESM), dist/index.cjs, dist/index.d.ts, dist/index.d.cts
npm test        # builds, compiles the tests, runs node:test against an in-process kernel stub
npm run check   # type-checks src and tests without emitting
```

Tests need no network: the stub listens on a loopback port chosen by the OS.

## License

Apache-2.0. Repository: https://github.com/rhs2/kernos (directory `sdk/typescript`).
