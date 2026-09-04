// Remove build outputs. Node only, no shell dependency.
import { rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
for (const dir of ["dist", ".test-build"]) {
  rmSync(join(root, dir), { recursive: true, force: true });
}
