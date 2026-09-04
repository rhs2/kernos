// A13: the TypeScript client round-trips a run against a live kernel.
//
// Usage: KERNOS_URL=http://127.0.0.1:17401 node tests/acceptance/ts_smoke.mjs
// Env:   KERNOS_URL (default http://127.0.0.1:17401), KERNOS_TOKEN (optional),
//        KERNOS_BUNDLE_ID (optional; otherwise the bundle is found by name).
// Needs sdk/typescript/dist to be built (npm install && npm run build there).
//
// Starts an `intake` run with an amount under the approval threshold, follows
// it to completion, lists pending approvals and reads the replay result. The
// last line of stdout is a JSON summary; the exit code is 0 only when every
// check held.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const distEntry = join(here, "..", "..", "sdk", "typescript", "dist", "index.js");
if (!existsSync(distEntry)) {
  console.error(`ts_smoke: ${distEntry} not found; run npm install && npm run build in sdk/typescript`);
  process.exit(2);
}
const { KernosClient, KernosError, KernosNetworkError } = await import(distEntry);

const baseUrl = process.env.KERNOS_URL ?? "http://127.0.0.1:17401";
const token = process.env.KERNOS_TOKEN;
const requestedBy = { id: "u-ana", role: "ap_clerk", manager: "u-tom" };
const deadline = setTimeout(() => {
  console.error("ts_smoke: timed out after 150 s");
  process.exit(1);
}, 150_000);

try {
  const k = new KernosClient({ baseUrl, token, pollMs: 250 });

  const health = await k.health();
  assert.equal(health.ok, true, "kernel health.ok");

  let bundleId = process.env.KERNOS_BUNDLE_ID;
  if (!bundleId) {
    const bundles = await k.bundles.list();
    const found = bundles.find((b) => b.name === "halcyon.finance.invoice_intake" && b.version === "1.0.0");
    assert.ok(found, "reference bundle halcyon.finance.invoice_intake@1.0.0 is applied");
    bundleId = found.bundle_id;
  }

  const remit = await k.remits.issue({
    tools: ["ledger.*", "http.get", "test.*"],
    scopes: ["sql:table:ledger_entries", "sql:table:vendors", "http:host:127.0.0.1", "test:*"],
    grants: [],
    spend: { tokens: 200000, usd: 2.0 },
    autonomy: "autonomous",
    ttl_seconds: 3600,
    policy_set: ["finance-default"],
    requested_by: requestedBy,
  });
  assert.match(remit.remit_id, /^rem_/);
  assert.match(remit.token, /^krt1\./);

  const invoiceId = `INV-TS-${Date.now().toString(36)}`;
  const run = await k.runs.start({
    bundle_id: bundleId,
    workflow: "intake",
    input: {
      invoice_id: invoiceId,
      text: `NORTHWIND DAIRY\nInvoice ${invoiceId}\nBill to: Halcyon Provisions\nWhole milk 30 crates\nTotal due: USD 1,750.00\n`,
      total: 1750.0,
      accounts: ["5100", "5200", "5300", "6100"],
    },
    remit_id: remit.remit_id,
    requested_by: requestedBy,
  });
  assert.match(run.run_id, /^run_/);
  assert.equal(run.state, "running");

  let count = 0;
  let last = null;
  let lastSeq = 0;
  const kinds = new Set();
  for await (const ev of k.runs.follow(run.run_id)) {
    count += 1;
    assert.equal(ev.seq, lastSeq + 1, `events arrive gap-free (seq ${ev.seq} after ${lastSeq})`);
    lastSeq = ev.seq;
    kinds.add(ev.kind);
    last = ev;
  }
  assert.ok(last, "follow yielded events");
  assert.equal(last.kind, "run.completed", `run ended with ${last.kind}`);
  assert.ok(kinds.has("run.created") && kinds.has("step.completed"), "log carries run.created and step.completed");

  const state = await k.runs.get(run.run_id);
  assert.equal(state.state, "completed");
  assert.equal(state.last_seq, lastSeq, "RunState.last_seq equals the last followed seq");
  assert.ok(state.steps.every((s) => s.state === "completed"), "every step completed");

  const approvals = await k.approvals.list({ state: "pending" });
  assert.ok(Array.isArray(approvals), "approvals.list returns a list");
  assert.ok(!approvals.some((a) => a.run_id === run.run_id), "the run needed no approval");

  const replay = await k.runs.replay(run.run_id);
  assert.equal(replay.chain_valid, true, "replay chain_valid");
  assert.equal(replay.state_matches, true, "replay state_matches");
  assert.deepEqual(replay.decision_mismatches, [], "replay decision_mismatches");
  assert.equal(replay.events, lastSeq, "replay counted every event");

  const page = await k.runs.events(run.run_id, { from_seq: 1, limit: 5 });
  assert.equal(page.events.length, 5);

  let notFound = null;
  try {
    await k.runs.get("run_00000000000000000000000000");
  } catch (err) {
    notFound = err;
  }
  assert.ok(notFound instanceof KernosError && notFound.status === 404, "unknown run maps to KernosError 404");
  assert.ok(KernosNetworkError, "KernosNetworkError is exported");

  console.log(
    JSON.stringify({
      ok: true,
      run_id: run.run_id,
      events: count,
      pending_approvals: approvals.length,
      replay: { chain_valid: replay.chain_valid, state_matches: replay.state_matches, decisions: replay.decisions },
    }),
  );
  clearTimeout(deadline);
  process.exit(0);
} catch (err) {
  clearTimeout(deadline);
  const detail = err instanceof Error ? `${err.name}: ${err.message}` : String(err);
  console.error(`ts_smoke: FAIL ${detail}`);
  if (err && typeof err === "object" && "code" in err) console.error(`  code=${err.code} status=${err.status} details=${JSON.stringify(err.details)}`);
  process.exit(1);
}
