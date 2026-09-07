/**
 * Checks that the committed `dist/` is what this source tree builds.
 *
 * `dist/` is checked in so that `cargo build` needs no Node toolchain, which
 * means a reviewer is asked to accept a bundle they cannot read. This rebuilds
 * it into a scratch directory and compares the two file by file, so that claim
 * can be checked in one command instead of trusted.
 *
 * A mismatch is not by itself evidence of anything wrong: it usually means the
 * bundle was built from different sources, or from a different Node or
 * dependency version. It does mean the two no longer correspond, which is the
 * thing worth knowing before a review.
 */

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const committed = join(root, "dist");
const rebuilt = join(root, ".dist-verify");

/** Every file under `dir`, keyed by its path with a sha256 of its contents. */
function hashTree(dir) {
  const files = new Map();
  const visit = (current) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const path = join(current, entry.name);
      if (entry.isDirectory()) {
        visit(path);
      } else {
        // Normalised to forward slashes so the report reads the same on
        // Windows as it does anywhere else.
        const name = relative(dir, path).split(sep).join("/");
        files.set(name, createHash("sha256").update(readFileSync(path)).digest("hex"));
      }
    }
  };
  visit(dir);
  return files;
}

if (!existsSync(committed)) {
  console.error("dist/ is missing. Run `npm run build` first.");
  process.exit(1);
}

// Invoked through node rather than through npx, which needs a shell on Windows
// and resolves differently depending on what else is installed.
const vite = join(root, "node_modules", "vite", "bin", "vite.js");
if (!existsSync(vite)) {
  console.error("vite is not installed. Run `npm install` first.");
  process.exit(1);
}

rmSync(rebuilt, { recursive: true, force: true });
try {
  execFileSync(process.execPath, [vite, "build", "--outDir", rebuilt, "--emptyOutDir"], {
    cwd: root,
    stdio: "inherit",
  });

  const expected = hashTree(committed);
  const actual = hashTree(rebuilt);
  const names = [...new Set([...expected.keys(), ...actual.keys()])].sort();

  const differences = names.flatMap((name) => {
    const before = expected.get(name);
    const after = actual.get(name);
    if (before === after) return [];
    if (before === undefined) return [`  only in the rebuild:  ${name}`];
    if (after === undefined) return [`  only in dist/:        ${name}`];
    return [`  contents differ:      ${name}`];
  });

  if (differences.length > 0) {
    console.error(`\ndist/ does not match a fresh build of this source:\n`);
    console.error(differences.join("\n"));
    console.error(`\nRebuild with \`npm run build\` and commit the result.\n`);
    process.exit(1);
  }

  console.log(`\ndist/ matches a fresh build: ${names.length} files identical.\n`);
} finally {
  rmSync(rebuilt, { recursive: true, force: true });
}
