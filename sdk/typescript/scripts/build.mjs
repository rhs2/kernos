// Build @kernos/sdk with two tsc passes and no bundler.
//
// Pass 1 (tsconfig.json):     ESM  -> dist/*.js and dist/*.d.ts
// Pass 2 (tsconfig.cjs.json): CJS  -> dist/cjs-tmp/*.js, then rewritten into dist/*.cjs
//
// The CommonJS files get their relative requires pointed at the .cjs siblings,
// and every .d.ts is mirrored as a .d.cts (with relative specifiers rewritten)
// so that `require("@kernos/sdk")` resolves types under node16 resolution too.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const dist = join(root, "dist");
const cjsTmp = join(dist, "cjs-tmp");
const tsc = join(root, "node_modules", "typescript", "bin", "tsc");

function run(label, args) {
  const res = spawnSync(process.execPath, [tsc, ...args], { cwd: root, stdio: "inherit" });
  if (res.status !== 0) {
    console.error(`build: ${label} failed with exit code ${res.status}`);
    process.exit(res.status ?? 1);
  }
}

if (!existsSync(tsc)) {
  console.error("build: typescript is not installed; run `npm install` first");
  process.exit(2);
}

rmSync(dist, { recursive: true, force: true });
mkdirSync(dist, { recursive: true });

run("esm pass", ["-p", "tsconfig.json"]);
run("cjs pass", ["-p", "tsconfig.cjs.json"]);

// Relative specifiers inside emitted code and declarations end in ".js";
// the CommonJS copies must point at ".cjs" siblings instead.
const relJs = /((?:require\(|from |import\()\s*["'])(\.\.?\/[^"']+)\.js(["'])/g;
const toCjs = (text) => text.replace(relJs, "$1$2.cjs$3");

for (const name of readdirSync(cjsTmp)) {
  if (!name.endsWith(".js")) continue;
  const text = readFileSync(join(cjsTmp, name), "utf8");
  writeFileSync(join(dist, name.replace(/\.js$/, ".cjs")), toCjs(text));
}
rmSync(cjsTmp, { recursive: true, force: true });

for (const name of readdirSync(dist)) {
  if (!name.endsWith(".d.ts")) continue;
  const text = readFileSync(join(dist, name), "utf8");
  writeFileSync(join(dist, name.replace(/\.d\.ts$/, ".d.cts")), toCjs(text));
}

const required = ["index.js", "index.cjs", "index.d.ts", "index.d.cts"];
for (const name of required) {
  if (!existsSync(join(dist, name))) {
    console.error(`build: expected dist/${name} to exist`);
    process.exit(1);
  }
}
console.log("build: dist/ ready (" + readdirSync(dist).length + " files)");
