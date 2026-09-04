import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { existsSync } from "node:fs";
import { Stub } from "./stub.js";

// The compiled test lives in .test-build/test/, so ../../dist is the package's dist/.
const distEsm = new URL("../../dist/index.js", import.meta.url);
const distCjs = new URL("../../dist/index.cjs", import.meta.url);
const distTypes = new URL("../../dist/index.d.ts", import.meta.url);

describe("built package", () => {
  it("ships ESM, CJS and declarations that expose the same surface", async () => {
    assert.ok(existsSync(distEsm), "dist/index.js missing; run npm run build");
    assert.ok(existsSync(distCjs), "dist/index.cjs missing; run npm run build");
    assert.ok(existsSync(distTypes), "dist/index.d.ts missing; run npm run build");
    const esm = (await import(distEsm.href)) as Record<string, unknown>;
    const cjs = createRequire(import.meta.url)(distCjs.pathname) as Record<string, unknown>;
    for (const name of ["KernosClient", "KernosError", "KernosNetworkError"]) {
      assert.equal(typeof esm[name], "function", `esm exports ${name}`);
      assert.equal(typeof cjs[name], "function", `cjs exports ${name}`);
    }
  });

  it("the CJS build talks to a kernel", async () => {
    const stub = await Stub.start();
    try {
      stub.on("GET", "/v1/health", { body: { ok: true, version: "0.1.0", uptime_s: 1, runs: { running: 0, parked: 0 } } });
      const cjs = createRequire(import.meta.url)(distCjs.pathname) as { KernosClient: new (o: { baseUrl: string }) => { health(): Promise<{ ok: boolean }> } };
      const k = new cjs.KernosClient({ baseUrl: stub.url });
      assert.equal((await k.health()).ok, true);
    } finally {
      await stub.close();
    }
  });
});
