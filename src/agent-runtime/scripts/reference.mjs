// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const pins = JSON.parse(readFileSync(resolve(packageRoot, "pi-source.json"), "utf8"));
export const referenceRoot = resolve(process.env.BK_CAT_PI_REFERENCE ?? resolve(packageRoot, "../../.build/pi-reference"));

/** Reviewed source entries at the immutable commit; no upstream file edits. */
export function referencePaths() {
  const upstream = JSON.parse(readFileSync(resolve(referenceRoot, "tsconfig.json"), "utf8"));
  const paths = Object.fromEntries(Object.entries(upstream.compilerOptions.paths).map(([key, entries]) =>
    [key, entries.map((entry) => resolve(referenceRoot, entry))],
  ));
  for (const [name, path] of Object.entries({
    client: "packages/mcp/src/client.ts",
    "input-schema": "packages/mcp/src/input-schema.ts",
    content: "packages/mcp/src/protocol/content.ts",
  })) paths[`@bk-cat/pi-reference/mcp/${name}`] = [resolve(referenceRoot, path)];
  return paths;
}

export function verifyReference() {
  const run = (...args) => execFileSync("git", ["-C", referenceRoot, ...args], { encoding: "utf8" }).trim();
  if (run("rev-parse", "HEAD") !== pins.commit) throw new Error("Pi checkout does not match pi-source.json");
  if (run("status", "--porcelain", "--untracked-files=no")) throw new Error("Pi tracked source has local modifications");
  for (const name of pins.packages) {
    const metadata = JSON.parse(readFileSync(resolve(referenceRoot, "packages", name, "package.json"), "utf8"));
    if (metadata.version !== pins.packageVersion) throw new Error(`Pinned package ${name} has version ${metadata.version}`);
  }
  return createRequire(resolve(referenceRoot, "package.json"));
}
