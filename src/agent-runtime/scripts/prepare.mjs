// SPDX-License-Identifier: AGPL-3.0-or-later

// Product packaging entry point. No model calls or publishing occur here.
// An existing reference checkout is verified, never reset or checked out.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, realpathSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { packageRoot, pins, referenceRoot, verifyReference } from "./reference.mjs";

const arguments_ = process.argv.slice(2);
if (arguments_.some((value) => value !== "--verify-only") || arguments_.length > 1) {
  throw new Error("Usage: node scripts/prepare.mjs [--verify-only]");
}
const verifyOnly = arguments_.includes("--verify-only");
const [major, minor] = process.versions.node.split(".").map(Number);
if (major < 22 || (major === 22 && minor < 19)) {
  throw new Error("The controlled runtime build requires Node.js 22.19.0 or newer");
}
if (pins.repository !== "https://github.com/earendil-works/pi.git" || !/^[0-9a-f]{40}$/.test(pins.commit)) {
  throw new Error("pi-source.json must name the reviewed Pi repository and a full commit");
}
const run = (program, args, cwd = referenceRoot) =>
  execFileSync(program, args, { cwd, stdio: "inherit" });
const canonical = (path) => {
  const value = realpathSync(path).replaceAll("\\", "/");
  return process.platform === "win32" ? value.toLowerCase() : value;
};

if (!existsSync(referenceRoot)) {
  if (verifyOnly) throw new Error("The pinned Pi reference checkout does not exist");
  mkdirSync(dirname(referenceRoot), { recursive: true });
  run("git", ["init", referenceRoot], dirname(referenceRoot));
  run("git", ["fetch", "--no-tags", "--depth=1", pins.repository, pins.commit]);
  run("git", ["checkout", "--detach", pins.commit]);
}
const top = execFileSync("git", ["-C", referenceRoot, "rev-parse", "--show-toplevel"], { encoding: "utf8" }).trim();
if (canonical(top) !== canonical(referenceRoot)) {
  throw new Error("BK_CAT_PI_REFERENCE must be the root of its own dedicated Pi checkout");
}
verifyReference();
console.log(`Verified original Pi ${pins.packageVersion} at ${pins.commit}`);

if (!verifyOnly) {
  // Invoke npm through its JavaScript entry point, without a shell. In
  // particular, spawning npm.cmd directly is not portable on Windows.
  const nodeDirectory = dirname(realpathSync(process.execPath));
  const npm = [
    process.env.npm_execpath,
    resolve(nodeDirectory, "node_modules/npm/bin/npm-cli.js"),
    resolve(nodeDirectory, "../lib/node_modules/npm/bin/npm-cli.js"),
  ].find((path) => path && path.endsWith("npm-cli.js") && existsSync(path));
  if (!npm) throw new Error("Install Node.js with npm to prepare the fixed runtime dependencies");
  // The upstream lockfile is authoritative. Lifecycle hooks, native addon
  // builds, model downloads and upstream release scripts are not needed.
  run(process.execPath, [npm, "ci", "--ignore-scripts", "--no-audit", "--no-fund"]);
  verifyReference();
  run(process.execPath, [resolve(packageRoot, "scripts/build.mjs")], packageRoot);
}
