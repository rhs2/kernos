import { after, before, beforeEach, describe, it } from "node:test";
import assert from "node:assert/strict";
import { KernosClient } from "../src/index.js";
import type { Event, EventKind } from "../src/index.js";
import { Stub } from "./stub.js";

let stub: Stub;
let k: KernosClient;

before(async () => {
  stub = await Stub.start();
  k = new KernosClient({ baseUrl: stub.url, pollMs: 10 });
});
after(async () => {
  await stub.close();
});
beforeEach(() => stub.reset());

function ev(seq: number, kind: EventKind, payload: Record<string, unknown> = {}): Event {
  return {
    schema: "kernos.events/1",
    run_id: "run_01",
    seq,
    ts: "2026-09-04T12:00:00.000Z",
    kind,
    actor: { type: "kernel", id: "kernel" },
    payload,
    prev_hash: "0".repeat(64),
    hash: "a".repeat(64),
  } as Event;
}

async function collect(gen: AsyncGenerator<Event, void, undefined>): Promise<Event[]> {
  const out: Event[] = [];
  for await (const e of gen) out.push(e);
  return out;
}

describe("runs.follow", () => {
  it("polls until run.completed and yields every event once, in order", async () => {
    const pages = [
      { events: [ev(1, "run.created"), ev(2, "step.scheduled", { step: "extract" })], next_seq: null },
      { events: [], next_seq: null },
      { events: [ev(3, "step.completed", { step: "extract" })], next_seq: null },
      { events: [ev(4, "run.completed", { output: {} })], next_seq: null },
    ];
    let i = 0;
    stub.on("GET", "/v1/runs/run_01/events", () => ({ body: pages[Math.min(i++, pages.length - 1)] }));
    const got = await collect(k.runs.follow("run_01"));
    assert.deepEqual(got.map((e) => e.seq), [1, 2, 3, 4]);
    assert.equal(got[3]?.kind, "run.completed");
    const froms = stub.requests.map((r) => r.query.get("from_seq"));
    assert.deepEqual(froms, ["1", "3", "3", "4"]);
    assert.equal(stub.requests[0]?.query.get("limit"), "500");
  });

  it("follows next_seq pages without waiting for the poll interval", async () => {
    const pages = [
      { events: [ev(1, "run.created")], next_seq: 2 },
      { events: [ev(2, "step.scheduled")], next_seq: 3 },
      { events: [ev(3, "run.completed")], next_seq: null },
    ];
    let i = 0;
    stub.on("GET", "/v1/runs/run_01/events", () => ({ body: pages[i++] }));
    const slow = new KernosClient({ baseUrl: stub.url, pollMs: 5000 });
    const started = Date.now();
    const got = await collect(slow.runs.follow("run_01", { limit: 1 }));
    assert.equal(got.length, 3);
    assert.ok(Date.now() - started < 2000, "pages must be read back to back");
    assert.deepEqual(stub.requests.map((r) => r.query.get("limit")), ["1", "1", "1"]);
  });

  it("stops on run.failed and run.abandoned and ignores events after the terminal one", async () => {
    for (const terminal of ["run.failed", "run.abandoned"] as const) {
      stub.reset();
      stub.on("GET", "/v1/runs/run_01/events", { body: { events: [ev(1, "run.created"), ev(2, terminal), ev(3, "note")], next_seq: null } });
      const got = await collect(k.runs.follow("run_01"));
      assert.deepEqual(got.map((e) => e.kind), ["run.created", terminal]);
      assert.equal(stub.requests.length, 1);
    }
  });

  it("starts from from_seq and skips earlier events the server repeats", async () => {
    stub.on("GET", "/v1/runs/run_01/events", { body: { events: [ev(2, "step.scheduled"), ev(5, "step.completed"), ev(6, "run.completed")], next_seq: null } });
    const got = await collect(k.runs.follow("run_01", { from_seq: 5 }));
    assert.deepEqual(got.map((e) => e.seq), [5, 6]);
    assert.equal(stub.requests[0]?.query.get("from_seq"), "5");
  });

  it("ends when the signal aborts", async () => {
    stub.on("GET", "/v1/runs/run_01/events", { body: { events: [], next_seq: null } });
    const ctl = new AbortController();
    const slow = new KernosClient({ baseUrl: stub.url, pollMs: 10000 });
    const gen = slow.runs.follow("run_01", { signal: ctl.signal });
    const first = gen.next();
    setTimeout(() => ctl.abort(), 30);
    const started = Date.now();
    const res = await first;
    assert.equal(res.done, true);
    assert.ok(Date.now() - started < 5000, "abort must interrupt the poll sleep");
    const again = k.runs.follow("run_01", { signal: AbortSignal.abort() });
    assert.equal((await again.next()).done, true);
  });

  it("propagates request errors to the consumer", async () => {
    stub.on("GET", "/v1/runs/run_01/events", { status: 404, body: { error: { code: "run_not_found", message: "no such run" } } });
    await assert.rejects(collect(k.runs.follow("run_01")), (e: unknown) => (e as { code?: string }).code === "run_not_found");
  });
});
