// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { packageRoot, referenceRoot, referencePaths, verifyReference } from "./reference.mjs";

const require = verifyReference();
const paths = referencePaths();
const output = resolve(packageRoot, "dist");
mkdirSync(output, { recursive: true });
const config = resolve(output, "tsconfig.check.json");
writeFileSync(config, JSON.stringify({
  extends: resolve(referenceRoot, "tsconfig.base.json"),
  compilerOptions: {
    noEmit: true, module: "NodeNext", moduleResolution: "NodeNext", paths,
    lib: ["ES2024", "DOM", "DOM.Iterable"],
    typeRoots: [resolve(referenceRoot, "node_modules/@types")],
  },
  include: [resolve(packageRoot, "src/**/*.ts").replaceAll("\\", "/")],
}, null, 2) + "\n");
const compilerPackage = require.resolve("typescript/package.json");
const compiler = JSON.parse(readFileSync(compilerPackage, "utf8"));
const compilerEntry = resolve(dirname(compilerPackage), compiler.bin.tsc);
const result = spawnSync(process.execPath, [compilerEntry, "-p", config], { stdio: "inherit" });
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
