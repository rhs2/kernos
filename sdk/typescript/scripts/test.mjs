// Run every compiled test file under .test-build/test with node:test.
// An explicit file list keeps this working on Node 18 through 25, where the
// semantics of directory and glob arguments to `node --test` changed.
import { spawnSync } from "node:child_process";
import { readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const testDir = join(root, ".test-build", "test");

function collect(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...collect(p));
    else if (name.endsWith(".test.js")) out.push(p);
  }
  return out.sort();
}

const files = collect(testDir);
if (files.length === 0) {
  console.error("test: no compiled test files found under .test-build/test");
  process.exit(1);
}
const res = spawnSync(process.execPath, ["--test", ...files], { cwd: root, stdio: "inherit" });
process.exit(res.status ?? 1);
