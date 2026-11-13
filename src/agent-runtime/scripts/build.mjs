// SPDX-License-Identifier: AGPL-3.0-or-later

import { createHash } from "node:crypto";
import { copyFileSync, cpSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { packageRoot, pins, referenceRoot, referencePaths, verifyReference } from "./reference.mjs";

const require = verifyReference();
const { build } = await import(pathToFileURL(require.resolve("esbuild")).href);
const output = resolve(packageRoot, "dist");
mkdirSync(output, { recursive: true });
const result = await build({
  absWorkingDir: referenceRoot,
  entryPoints: [resolve(packageRoot, "src/index.ts")],
  tsconfigRaw: { compilerOptions: { paths: referencePaths() } },
  nodePaths: [resolve(referenceRoot, "node_modules")],
  bundle: true,
  platform: "browser",
  target: "es2022",
  format: "iife",
  globalName: "BKCatRuntime",
  metafile: true,
  legalComments: "eof",
  outfile: resolve(output, "runtime.js"),
  banner: { js: "// BK CAT controlled runtime. AGPL-3.0-or-later; bundled Pi original components: MIT, see PI-LICENSE.txt." },
});
await build({
  entryPoints: [resolve(packageRoot, "src/browser-entry.ts")],
  bundle: true, platform: "browser", target: "es2022", format: "iife", globalName: "BKCatBootstrap",
  legalComments: "eof", outfile: resolve(output, "bootstrap.js"),
});
copyFileSync(resolve(packageRoot, "assets/index.html"), resolve(output, "index.html"));
const normalized = (path) => path.replaceAll("\\", "/");
const inputs = Object.keys(result.metafile.inputs).map(normalized);
for (const required of [
  "packages/durable/src/harness/harness.ts", "packages/durable/src/storage/sqlite/storage.ts",
  "packages/ai/src/models.ts", "packages/mcp/src/client.ts",
]) {
  if (!inputs.some((path) => path === required || path.endsWith(`/${required}`))) {
    throw new Error(`Browser bundle omitted original Pi component ${required}`);
  }
}
const contributes = Object.values(result.metafile.outputs).flatMap((entry) =>
  Object.entries(entry.inputs).filter(([, size]) => size.bytesInOutput > 0).map(([path]) => normalized(path)),
);
for (const forbidden of [
  "/storage/indexeddb.ts", "/storage/sqlite/node.ts", "/env/node.ts", "/transports/stdio.ts",
  "/pi-agent-core/", "/packages/agent/src/agent.ts", "/packages/ai/src/compat.ts",
]) {
  if (contributes.some((path) => path.includes(forbidden))) throw new Error(`Unreviewed runtime dependency ${forbidden}`);
}
copyFileSync(resolve(referenceRoot, "LICENSE"), resolve(output, "PI-LICENSE.txt"));
cpSync(resolve(referenceRoot, "packages/mcp/THIRD_PARTY_NOTICES"), resolve(output, "PI-MCP-THIRD-PARTY-NOTICES"), { recursive: true });
// cargo-apk includes this directory through concat-android's assets metadata.
// Keep complete legal texts in the APK as files, not only a bundle comment.
const androidLicenses = resolve(output, "android-assets/licenses");
mkdirSync(androidLicenses, { recursive: true });
copyFileSync(resolve(packageRoot, "../../LICENSE"), resolve(androidLicenses, "BK-CAT-LICENSE.txt"));
copyFileSync(resolve(packageRoot, "../../THIRD_PARTY_NOTICES.md"), resolve(androidLicenses, "THIRD_PARTY_NOTICES.md"));
copyFileSync(resolve(output, "PI-LICENSE.txt"), resolve(androidLicenses, "PI-LICENSE.txt"));
cpSync(resolve(output, "PI-MCP-THIRD-PARTY-NOTICES"), resolve(androidLicenses, "PI-MCP-THIRD-PARTY-NOTICES"), { recursive: true });
writeFileSync(resolve(androidLicenses, "PI-SOURCE.txt"), [
  "Original Pi components and their MCP SDK notices are copied without text changes.",
  "Repository: " + pins.repository,
  "Commit: " + pins.commit,
  "Component version: " + pins.packageVersion,
  "",
].join("\n"));
const bundle = readFileSync(resolve(output, "runtime.js"));
const files = Object.fromEntries(["index.html", "runtime.js", "bootstrap.js"].map((name) => {
  const content = readFileSync(resolve(output, name));
  return [name, { bytes: content.length, sha256: createHash("sha256").update(content).digest("hex") }];
}));
writeFileSync(resolve(output, "manifest.json"), JSON.stringify({
  piCommit: pins.commit,
  piVersion: pins.packageVersion,
  runtimeVersion: "0.1.0",
  protocolVersion: pins.mcpProtocolVersion,
  sqliteSchemaVersion: pins.sqliteSchemaVersion,
  format: "browser-iife",
  target: "es2022",
  bytes: bundle.length,
  sha256: createHash("sha256").update(bundle).digest("hex"),
  files,
}, null, 2) + "\n");
writeFileSync(resolve(output, "metafile.json"), JSON.stringify(result.metafile, null, 2) + "\n");
console.log(`Built original Pi ${pins.packageVersion} at ${pins.commit}: ${bundle.length} bytes`);
