import { after, before, beforeEach, describe, it } from "node:test";
import assert from "node:assert/strict";
import { KernosClient, KernosError, KernosNetworkError } from "../src/index.js";
import type { Bundle, BundleSignature, RunState, Event } from "../src/index.js";
import { Stub } from "./stub.js";

let stub: Stub;
let k: KernosClient;

before(async () => {
  stub = await Stub.start();
  k = new KernosClient({ baseUrl: stub.url + "/", token: "secret-token" });
});
after(async () => {
  await stub.close();
});
beforeEach(() => stub.reset());

const requestedBy = { id: "u-ana", role: "ap_clerk", manager: "u-tom" };

const bundle: Bundle = {
  format: "kernos.bundle/1",
  name: "halcyon.finance.invoice_intake",
  version: "1.0.0",
  department: "finance",
  policies: ["finance-default"],
  tools: [{ id: "ledger.post_entry", description: "Post a journal entry", writes: true }],
  prompts: { extract: { system: "Extract fields.", user: "Invoice text:\n{{input.text}}" } },
  workflows: {
    intake: {
      input_schema: { type: "object", required: ["invoice_id"], properties: { invoice_id: { type: "string" } } },
      steps: [
        { id: "extract", kind: "model", tier: "standard", effort: "low", prompt: "extract", output_schema: { type: "object" } },
        {
          id: "post",
          kind: "tool",
          tool: "ledger.post_entry",
          args: { invoice_id: { $ref: "input.invoice_id" } },
          idempotency_key: "{{input.invoice_id}}",
        },
      ],
    },
  },
};
const signature: BundleSignature = { key_id: "key_pub", algorithm: "ed25519", signature: "c2ln", sha256: "00" };

describe("construction", () => {
  it("strips a trailing slash from baseUrl and applies the default pollMs", () => {
    assert.equal(k.baseUrl, stub.url);
    assert.equal(k.pollMs, 500);
    const custom = new KernosClient({ baseUrl: stub.url, pollMs: 25 });
    assert.equal(custom.pollMs, 25);
  });

  it("throws when no fetch is available", () => {
    const saved = globalThis.fetch;
    // @ts-expect-error simulate a runtime without fetch
    globalThis.fetch = undefined;
    try {
      assert.throws(() => new KernosClient({ baseUrl: stub.url }), /global fetch/);
    } finally {
      globalThis.fetch = saved;
    }
  });
});

describe("headers", () => {
  it("sends Authorization Bearer when a token is set", async () => {
    stub.on("GET", "/v1/health", { body: { ok: true, version: "0.1.0", uptime_s: 1, runs: { running: 0, parked: 0 } } });
    await k.health();
    assert.equal(stub.last().headers.authorization, "Bearer secret-token");
    assert.equal(stub.last().headers.accept, "application/json");
  });

  it("sends no Authorization header without a token", async () => {
    stub.on("GET", "/v1/health", { body: { ok: true, version: "0.1.0", uptime_s: 1, runs: { running: 0, parked: 0 } } });
    const anon = new KernosClient({ baseUrl: stub.url, headers: { "x-trace": "abc" } });
    await anon.health();
    assert.equal(stub.last().headers.authorization, undefined);
    assert.equal(stub.last().headers["x-trace"], "abc");
  });

  it("accepts a custom fetch implementation", async () => {
    let seen = "";
    const custom = new KernosClient({
      baseUrl: "http://example.invalid",
      fetch: async (input) => {
        seen = input;
        return new Response(JSON.stringify({ ok: true, version: "x", uptime_s: 0, runs: { running: 0, parked: 0 } }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      },
    });
    const h = await custom.health();
    assert.equal(h.version, "x");
    assert.equal(seen, "http://example.invalid/v1/health");
  });
});

describe("health and keys", () => {
  it("health()", async () => {
    const body = { ok: true, version: "0.1.0", uptime_s: 12, runs: { running: 1, parked: 0 } };
    stub.on("GET", "/v1/health", { body });
    assert.deepEqual(await k.health(), body);
    assert.equal(stub.last().method, "GET");
    assert.equal(stub.last().path, "/v1/health");
  });

  it("keys()", async () => {
    const body = { key_id: "key_01", algorithm: "ed25519", public_key: "AAAA" };
    stub.on("GET", "/v1/keys", { body });
    assert.deepEqual(await k.keys(), body);
  });

  it("metrics() returns Prometheus text", async () => {
    stub.on("GET", "/v1/metrics", { raw: "kernos_runs{state=\"running\"} 1\n" });
    const text = await k.metrics();
    assert.match(text, /kernos_runs/);
    assert.equal(stub.last().headers.accept, "text/plain");
  });
});

describe("bundles", () => {
  it("apply posts {bundle, signature}", async () => {
    const body = { bundle_id: "bnd_01", name: bundle.name, version: "1.0.0" };
    stub.on("POST", "/v1/bundles", { status: 201, body });
    assert.deepEqual(await k.bundles.apply(bundle, signature), body);
    const req = stub.last();
    assert.equal(req.headers["content-type"], "application/json");
    assert.deepEqual(req.body, { bundle, signature });
  });

  it("list and get", async () => {
    const summary = [{ bundle_id: "bnd_01", name: bundle.name, version: "1.0.0", department: "finance", workflows: ["intake"], created_at: "2026-09-04T12:00:00.000Z" }];
    stub.on("GET", "/v1/bundles", { body: summary });
    stub.on("GET", "/v1/bundles/bnd_01", { body: { ...bundle, bundle_id: "bnd_01", signature } });
    assert.deepEqual(await k.bundles.list(), summary);
    const got = await k.bundles.get("bnd_01");
    assert.equal(got.bundle_id, "bnd_01");
    assert.equal(got.workflows.intake?.steps.length, 2);
    assert.equal(stub.last().path, "/v1/bundles/bnd_01");
  });
});

describe("policies", () => {
  it("apply", async () => {
    const body = { policy_id: "pol_01", name: "finance-default", version: 1 };
    stub.on("POST", "/v1/policies", { status: 201, body });
    const req = { name: "finance-default", version: 1, source: 'policy "finance-default"\n' };
    assert.deepEqual(await k.policies.apply(req), body);
    assert.deepEqual(stub.last().body, req);
  });

  it("test", async () => {
    const body = { cases: 2, flips: [{ index: 1, a: "approval_required", b: "allow", rule_a: "finance-default@1#0", rule_b: "default" }] };
    stub.on("POST", "/v1/policies/test", { body });
    const req = {
      policy_a: { name: "finance-default", version: 1 },
      policy_b: { source: 'policy "x"\n' },
      corpus: [
        { action: { kind: "payment.issue", amount: 100, writes_to_system_of_record: true }, run: { workflow: "intake" } },
        { action: { kind: "payment.issue", amount: 7000, writes_to_system_of_record: true }, run: { workflow: "intake" } },
      ],
    };
    const res = await k.policies.test(req);
    assert.equal(res.flips[0]?.b, "allow");
    assert.deepEqual(stub.last().body, req);
  });

  it("list, get, getVersion", async () => {
    stub.on("GET", "/v1/policies", { body: [{ policy_id: "pol_01", name: "finance-default", version: 1, created_at: "t" }] });
    stub.on("GET", "/v1/policies/finance-default", { body: [{ policy_id: "pol_01", name: "finance-default", version: 1, created_at: "t" }] });
    stub.on("GET", "/v1/policies/finance-default/1", { body: { policy_id: "pol_01", name: "finance-default", version: 1, created_at: "t", source: "policy" } });
    assert.equal((await k.policies.list()).length, 1);
    assert.equal((await k.policies.get("finance-default"))[0]?.version, 1);
    assert.equal((await k.policies.getVersion("finance-default", 1)).source, "policy");
    assert.equal(stub.last().path, "/v1/policies/finance-default/1");
  });
});

describe("remits", () => {
  const issue = {
    tools: ["ledger.*"],
    scopes: ["sql:table:ledger_entries"],
    grants: [],
    spend: { tokens: 1000, usd: 1 },
    autonomy: "supervised" as const,
    ttl_seconds: 3600,
    policy_set: ["finance-default"],
    requested_by: requestedBy,
  };

  it("issue", async () => {
    const body = { remit_id: "rem_01", token: "krt1.a.b.c", expires_at: "t" };
    stub.on("POST", "/v1/remits", { status: 201, body });
    assert.deepEqual(await k.remits.issue(issue), body);
    assert.deepEqual(stub.last().body, issue);
  });

  it("derive", async () => {
    const body = { remit_id: "rem_02", parent_id: "rem_01", token: "krt1.x.y.z", expires_at: "t" };
    stub.on("POST", "/v1/remits/rem_01/derive", { status: 201, body });
    assert.deepEqual(await k.remits.derive("rem_01", { autonomy: "propose", spend: { usd: 0.5 } }), body);
    assert.deepEqual(stub.last().body, { autonomy: "propose", spend: { usd: 0.5 } });
  });

  it("get", async () => {
    const body = { rid: "rem_01", iss: "key_01", iat: 1, nbf: 1, exp: 2, ...issue, parent_id: null, run_id: null };
    stub.on("GET", "/v1/remits/rem_01", { body });
    const r = await k.remits.get("rem_01");
    assert.equal(r.rid, "rem_01");
    assert.equal(r.autonomy, "supervised");
  });
});

describe("runs", () => {
  const state: RunState = {
    run_id: "run_01",
    state: "completed",
    bundle: { id: "bnd_01", name: bundle.name, version: "1.0.0" },
    workflow: "intake",
    input: { invoice_id: "inv-1" },
    remit_id: "rem_01",
    requested_by: requestedBy,
    steps: [],
    budget: { ceiling_tokens: 1, ceiling_usd: 1, soft_ratio: 0.8, used_tokens: 0, used_usd: 0, soft_hit: false, exceeded: false },
    pending_approval: null,
    decisions: [],
    compensations: [],
    output: null,
    error: null,
    needs_human: false,
    last_seq: 3,
  };

  it("start", async () => {
    stub.on("POST", "/v1/runs", { status: 201, body: { run_id: "run_01", state: "running" } });
    const req = { bundle_id: "bnd_01", workflow: "intake", input: { invoice_id: "inv-1" }, remit_id: "rem_01", requested_by: requestedBy };
    assert.deepEqual(await k.runs.start(req), { run_id: "run_01", state: "running" });
    assert.deepEqual(stub.last().body, req);
  });

  it("get", async () => {
    stub.on("GET", "/v1/runs/run_01", { body: state });
    assert.deepEqual(await k.runs.get("run_01"), state);
  });

  it("list encodes the query string and omits undefined", async () => {
    stub.on("GET", "/v1/runs", { body: { runs: [{ run_id: "run_01", state: "parked" }], next: null } });
    const res = await k.runs.list({ state: "parked", department: "finance", limit: 50, after: undefined });
    assert.equal(res.runs[0]?.run_id, "run_01");
    const q = stub.last().query;
    assert.equal(q.get("state"), "parked");
    assert.equal(q.get("department"), "finance");
    assert.equal(q.get("limit"), "50");
    assert.equal(q.has("after"), false);
    stub.on("GET", "/v1/runs", { body: { runs: [], next: null } });
    await k.runs.list();
    assert.equal([...stub.last().query.keys()].length, 0);
  });

  it("events with from_seq and limit", async () => {
    const ev: Event = {
      schema: "kernos.events/1",
      run_id: "run_01",
      seq: 1,
      ts: "t",
      kind: "run.created",
      actor: { type: "kernel", id: "kernel" },
      payload: {
        bundle_id: "bnd_01",
        bundle_name: bundle.name,
        bundle_version: "1.0.0",
        workflow: "intake",
        input: {},
        remit_id: "rem_01",
        requested_by: requestedBy,
        budget: { tokens: 1, usd: 1, soft_ratio: 0.8 },
      },
      prev_hash: "0".repeat(64),
      hash: "a".repeat(64),
    };
    stub.on("GET", "/v1/runs/run_01/events", { body: { events: [ev], next_seq: null } });
    const page = await k.runs.events("run_01", { from_seq: 1, limit: 10 });
    assert.equal(page.events[0]?.kind, "run.created");
    if (page.events[0]?.kind === "run.created") assert.equal(page.events[0].payload.workflow, "intake");
    assert.equal(stub.last().query.get("from_seq"), "1");
    assert.equal(stub.last().query.get("limit"), "10");
  });

  it("replay posts with no body", async () => {
    const body = { chain_valid: true, events: 3, state_matches: true, decisions: 0, decision_mismatches: [], chain_errors: [], state };
    stub.on("POST", "/v1/runs/run_01/replay", { body });
    assert.equal((await k.runs.replay("run_01")).chain_valid, true);
    assert.equal(stub.last().rawBody, "");
    assert.equal(stub.last().headers["content-type"], undefined);
  });

  it("abandon and resume", async () => {
    stub.on("POST", "/v1/runs/run_01/abandon", { status: 202, body: { compensations_scheduled: 2 } });
    stub.on("POST", "/v1/runs/run_01/resume", { body: { run_id: "run_01", run_state: "running" } });
    const ab = await k.runs.abandon("run_01", { reason: "operator", actor: { id: "u-tom", role: "finance_admin" } });
    assert.equal(ab.compensations_scheduled, 2);
    assert.deepEqual(stub.last().body, { reason: "operator", actor: { id: "u-tom", role: "finance_admin" } });
    const rs = await k.runs.resume("run_01", { actor: { id: "u-tom" } });
    assert.equal(rs.run_state, "running");
  });

  it("appendEvent sends the lease and remit headers", async () => {
    stub.on("POST", "/v1/runs/run_01/events", { status: 201, body: { seq: 9, hash: "b".repeat(64) } });
    const res = await k.runs.appendEvent(
      "run_01",
      { kind: "note", payload: { text: "hello" }, actor: { type: "worker", id: "wrk-1" } },
      { lease: "lse_01", remit: "krt1.a.b.c" },
    );
    assert.equal(res.seq, 9);
    assert.equal(stub.last().headers["x-kernos-lease"], "lse_01");
    assert.equal(stub.last().headers["x-kernos-remit"], "krt1.a.b.c");
    assert.deepEqual(stub.last().body, { kind: "note", payload: { text: "hello" }, actor: { type: "worker", id: "wrk-1" } });
  });
});

describe("leases", () => {
  it("acquire returns null on 204", async () => {
    stub.on("POST", "/v1/leases", { status: 204 });
    assert.equal(await k.leases.acquire({ worker_id: "wrk-1", kinds: ["model"], ttl_seconds: 30 }), null);
    assert.deepEqual(stub.last().body, { worker_id: "wrk-1", kinds: ["model"], ttl_seconds: 30 });
  });

  it("acquire returns the lease on 200", async () => {
    const lease = { lease_id: "lse_01", run_id: "run_01", step: "extract", attempt: 1, expires_at: "t", heartbeat_seconds: 10, step_def: { id: "extract", kind: "model" }, context: { pacing: false } };
    stub.on("POST", "/v1/leases", { body: lease });
    const got = await k.leases.acquire({ worker_id: "wrk-1", kinds: ["model", "tool"] });
    assert.equal(got?.lease_id, "lse_01");
  });

  it("heartbeat, complete, fail, propose", async () => {
    stub.on("POST", "/v1/leases/lse_01/heartbeat", { body: { expires_at: "t2" } });
    stub.on("POST", "/v1/leases/lse_01/complete", { body: { run_state: "running", next_step: "code" } });
    stub.on("POST", "/v1/leases/lse_01/fail", { body: { outcome: "retry_scheduled", delay_ms: 500 } });
    stub.on("POST", "/v1/leases/lse_01/actions", { body: { action_id: "act_01", decision: "allow", rule: "default" } });
    assert.equal((await k.leases.heartbeat("lse_01")).expires_at, "t2");
    assert.equal((await k.leases.complete("lse_01", { output: { a: 1 }, usage: { tokens: 10, usd: 0.001 } })).next_step, "code");
    assert.deepEqual(stub.last().body, { output: { a: 1 }, usage: { tokens: 10, usd: 0.001 } });
    assert.equal((await k.leases.fail("lse_01", { error: { code: "x", message: "y" }, deterministic: false })).outcome, "retry_scheduled");
    const decision = await k.leases.propose("lse_01", { action: { kind: "payment.issue", amount: 10, writes_to_system_of_record: true } });
    assert.equal(decision.decision, "allow");
  });
});

describe("approvals", () => {
  it("list with query", async () => {
    const body = [{ approval_id: "apr_01", run_id: "run_01", action_id: "act_01", action: { kind: "payment.issue", writes_to_system_of_record: true }, approver: { type: "role", value: "finance_admin" }, requested_at: "t", due_at: "t", escalations: 0 }];
    stub.on("GET", "/v1/approvals", { body });
    const res = await k.approvals.list({ state: "pending", approver: "role:finance_admin" });
    assert.equal(res[0]?.approval_id, "apr_01");
    assert.equal(stub.last().query.get("state"), "pending");
    assert.equal(stub.last().query.get("approver"), "role:finance_admin");
  });

  it("decide", async () => {
    stub.on("POST", "/v1/approvals/apr_01", { body: { run_id: "run_01", run_state: "running" } });
    const req = { decision: "approved" as const, actor: { id: "u-tom", role: "finance_admin" }, reason: "Checked the delivery note" };
    assert.deepEqual(await k.approvals.decide("apr_01", req), { run_id: "run_01", run_state: "running" });
    assert.deepEqual(stub.last().body, req);
  });
});

describe("error mapping", () => {
  it("maps a JSON error envelope to KernosError", async () => {
    stub.on("POST", "/v1/remits/rem_01/derive", {
      status: 422,
      body: { error: { code: "remit_widens", message: "tools widens the parent", details: { field: "tools" } } },
    });
    await assert.rejects(
      k.remits.derive("rem_01", { tools: ["ledger.*"] }),
      (err: unknown) => {
        assert.ok(err instanceof KernosError);
        assert.ok(err instanceof Error);
        assert.equal(err.name, "KernosError");
        assert.equal(err.status, 422);
        assert.equal(err.code, "remit_widens");
        assert.equal(err.message, "tools widens the parent");
        assert.deepEqual(err.details, { field: "tools" });
        assert.equal(err.request.method, "POST");
        assert.equal(err.request.url, `${stub.url}/v1/remits/rem_01/derive`);
        return true;
      },
    );
  });

  it("maps 403 and 409 codes", async () => {
    stub.on("POST", "/v1/approvals/apr_01", { status: 403, body: { error: { code: "not_the_approver", message: "no" } } });
    await assert.rejects(k.approvals.decide("apr_01", { decision: "approved", actor: { id: "u-ana", role: "ap_clerk" }, reason: "why" }), (e: unknown) => e instanceof KernosError && e.code === "not_the_approver" && e.status === 403 && Object.keys(e.details).length === 0);
    stub.on("POST", "/v1/runs", { status: 409, body: { error: { code: "remit_bound", message: "bound", details: {} } } });
    await assert.rejects(k.runs.start({ bundle_id: "b", workflow: "intake", input: {}, remit_id: "rem_01", requested_by: requestedBy }), (e: unknown) => e instanceof KernosError && e.code === "remit_bound");
  });

  it("falls back to http_<status> when the body is not an error envelope", async () => {
    stub.on("GET", "/v1/health", { status: 500, raw: "<html>boom</html>" });
    await assert.rejects(k.health(), (e: unknown) => {
      assert.ok(e instanceof KernosError);
      assert.equal(e.status, 500);
      assert.equal(e.code, "http_500");
      assert.match(e.message, /boom/);
      return true;
    });
    stub.on("GET", "/v1/keys", { status: 404 });
    await assert.rejects(k.keys(), (e: unknown) => e instanceof KernosError && e.code === "http_404" && e.status === 404);
    stub.on("GET", "/v1/bundles", { status: 400, body: { message: "no envelope" } });
    await assert.rejects(k.bundles.list(), (e: unknown) => e instanceof KernosError && e.code === "http_400");
  });

  it("reports an unparseable or empty 2xx body as response_invalid", async () => {
    stub.on("GET", "/v1/health", { raw: "not json" });
    await assert.rejects(k.health(), (e: unknown) => e instanceof KernosError && e.code === "response_invalid" && e.status === 200);
    stub.on("GET", "/v1/keys", { status: 200 });
    await assert.rejects(k.keys(), (e: unknown) => e instanceof KernosError && e.code === "response_invalid");
  });

  it("throws KernosNetworkError when the connection fails", async () => {
    const probe = await Stub.start();
    const deadUrl = probe.url;
    await probe.close();
    const dead = new KernosClient({ baseUrl: deadUrl });
    await assert.rejects(dead.health(), (e: unknown) => {
      assert.ok(e instanceof KernosNetworkError);
      assert.equal(e.name, "KernosNetworkError");
      assert.ok(e.cause !== undefined);
      assert.equal(e.request.method, "GET");
      assert.match(e.message, /GET http:\/\/127\.0\.0\.1/);
      return true;
    });
  });

  it("throws KernosNetworkError when fetch rejects", async () => {
    const boom = new KernosClient({
      baseUrl: "http://example.invalid",
      fetch: async () => {
        throw new TypeError("fetch failed");
      },
    });
    await assert.rejects(boom.keys(), (e: unknown) => e instanceof KernosNetworkError && e.cause instanceof TypeError);
  });
});
