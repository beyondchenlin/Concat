// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
// Reproducible offline extraction of authored capacities from the pinned Pi
// commit. Live catalogue hydration is deliberately outside this operation.
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import vm from "node:vm";
import { packageRoot, pins, referencePaths, referenceRoot, verifyReference } from "./reference.mjs";

const require = verifyReference();
const { build } = await import(pathToFileURL(require.resolve("esbuild")).href);
const providers = ["deepseek", "openai", "anthropic", "google", "minimax", "minimax-cn", "moonshotai", "moonshotai-cn", "zai", "openrouter"];
const entry = [
  `import { buildCatalog } from ${JSON.stringify(resolve(referenceRoot, "packages/ai/catalog/build.ts"))};`,
  ...providers.map((provider, index) => `import provider${index} from ${JSON.stringify(resolve(referenceRoot, `packages/ai/catalog/providers/${provider}.ts`))};`),
  `export const catalog = buildCatalog([${providers.map((_, index) => `provider${index}`).join(",")}], {`,
  `modelsDev: {}, modelsDevDecisions: {}, openRouter: { chat: [], images: [], classifiers: [] },`,
  `aiGateway: { chat: [], classifiers: [] }, nvidiaModelIds: new Map(), radius: [] }, false);`,
].join("\n");
const result = await build({
  absWorkingDir: referenceRoot, stdin: { contents: entry, loader: "ts", resolveDir: referenceRoot },
  tsconfigRaw: { compilerOptions: { paths: referencePaths() } }, nodePaths: [resolve(referenceRoot, "node_modules")],
  bundle: true, platform: "browser", target: "es2022", format: "iife", globalName: "PinnedCapacity", write: false,
});
const context = vm.createContext({ structuredClone, URL, console: { log() {}, warn() {}, error() {} },
  fetch() { throw new Error("Offline capacity extraction must never fetch catalogue data"); } });
vm.runInContext(result.outputFiles[0].text, context);
const models = Object.values(context.PinnedCapacity.catalog).flatMap((provider) => provider.chat)
  .filter((model) => Number.isSafeInteger(model.contextWindow) && model.contextWindow > 0)
  .map((model) => ({ provider: model.provider, model: model.id, protocol: model.api,
    endpoint: model.baseUrl.replace(/\/+$/u, ""), contextWindow: model.contextWindow,
    source: `locked-pi-${pins.packageVersion}:${pins.commit}:catalog/providers/${model.provider}.ts#${model.id}` }))
  .sort((left, right) => `${left.provider}/${left.model}`.localeCompare(`${right.provider}/${right.model}`));
const output = JSON.stringify({ version: pins.packageVersion, commit: pins.commit,
  scope: "authored-models-only; no live upstream metadata", models }, null, 2) + "\n";
const target = resolve(packageRoot, "src/pinned-model-context.json");
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== output) throw new Error("Pinned model context artifact differs from its original source");
} else writeFileSync(target, output);
console.log(`Verified ${models.length} authored model capacities at pinned Pi ${pins.packageVersion} (${pins.commit}).`);
