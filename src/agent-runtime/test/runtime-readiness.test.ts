// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import { browserContext } from "./platform-fixture.ts";

const component = "a".repeat(64);
const channel = "bk-cat-runtime-v1";
const source = readFileSync(new URL("../../crates/concat/src/embedded/transport.js", import.meta.url), "utf8")
  .replace("__BKCAT_COMPONENT__", JSON.stringify(component));

function fixture(before = "") {
  const sent: string[] = [];
  const browser = browserContext({ BKCatAndroid: { postMessage(value: string) { sent.push(value); } } });
  if (before) vm.runInContext(before, browser.context);
  vm.runInContext(source, browser.context, { filename: "transport.js" });
  return { ...browser, sent };
}

test("missing required capabilities refuse both bundles with a fixed native-bound reason", () => {
  for (const [remove, expected] of [
    ["queueMicrotask = undefined", "web-platform"],
    ["WeakRef = undefined", "weak-references"],
    ["FinalizationRegistry = undefined", "weak-references"],
    ["crypto = undefined", "secure-random"],
    ["AbortController = undefined", "cancellation"],
    ["Array.prototype.at = undefined", "collection-methods"],
    ["Object.hasOwn = undefined", "collection-methods"],
    ["Promise.withResolvers = undefined", "promise-resolvers"],
    ["structuredClone = undefined", "structured-clone"],
  ]) {
    const f = fixture(remove);
    assert.equal(f.scripts.length, 0, remove);
    assert.equal(f.context.BKCatNative, undefined, remove);
    assert.deepEqual(f.sent.map((message) => JSON.parse(message)), [
      { channel, component, event: "runtime.loadFailed", code: expected },
    ], remove);
    f.emit("error", { message: "credential-secret" });
    assert.equal(f.sent.length, 1, "retired startup cannot send a second report");
  }
});

test("the loader waits for the original runtime before loading bootstrap and invents no success", () => {
  const f = fixture();
  assert.deepEqual(f.scripts.map((script) => script.src), ["/runtime.js"]);
  assert.equal(f.sent.length, 0);
  f.context.BKCatRuntime = { originalFixture: true };
  f.scripts[0]!.onload();
  f.scripts[0]!.onload();
  assert.deepEqual(f.scripts.map((script) => script.src), ["/runtime.js", "/bootstrap.js"]);
  assert.equal(f.sent.length, 0);
  // Only this explicit stand-in for the actual bootstrap announces loading.
  f.context.BKCatNative.postMessage(JSON.stringify({ channel, event: "runtime.loaded" }));
  f.scripts[1]!.onload();
  assert.equal(f.sent.length, 1);
  assert.equal(JSON.parse(f.sent[0]!).event, "runtime.loaded");
  f.emit("error", { message: "later-runtime-error" });
  assert.equal(f.sent.length, 1, "startup diagnostics detach after actual loading");
});

test("a real script parse error reports only a fixed code and cannot load bootstrap", () => {
  const f = fixture();
  let parseError: unknown;
  try { vm.runInContext("class Broken { #unfinished( {", f.context); }
  catch (error) { parseError = error; }
  assert.ok(parseError);
  // A browser dispatches this event when the script parser rejects a bundle.
  f.emit("error", { error: parseError, message: "credential-secret", filename: "private-file" });
  f.context.BKCatRuntime = {};
  f.scripts[0]!.onload();
  f.scripts[0]!.onerror();
  assert.deepEqual(f.sent.map((message) => JSON.parse(message)), [
    { channel, component, event: "runtime.loadFailed", code: "script-execution" },
  ]);
  assert.equal(f.scripts.length, 1, "late resource completion cannot revive failed loading");
  assert.throws(() => f.context.BKCatNative.postMessage(JSON.stringify({ channel, event: "runtime.loaded" })), /retired/);
});

test("missing runtime, resource failure and absent bootstrap announcements fail distinctly", () => {
  for (const reason of ["runtime-missing", "script-resource", "bootstrap-missing"]) {
    const f = fixture();
    if (reason === "script-resource") f.scripts[0]!.onerror();
    else if (reason === "runtime-missing") f.scripts[0]!.onload();
    else {
      f.context.BKCatRuntime = {};
      f.scripts[0]!.onload();
      f.scripts[1]!.onload();
    }
    assert.equal(f.sent.length, 1);
    assert.equal(JSON.parse(f.sent[0]!).code, reason);
    assert.equal(f.sent.some((message) => message.includes("runtime.loaded")), false);
  }
});

test("page retirement fences pending loads, queued messages and late script errors", () => {
  const f = fixture();
  let deliveries = 0;
  f.context.BKCatNative.addEventListener("message", () => deliveries++);
  f.emit("pagehide");
  f.context.BKCatRuntime = {};
  f.scripts[0]!.onload();
  f.scripts[0]!.onerror();
  f.emit("error", { message: "late" });
  f.context.__BKCatDeliver(JSON.stringify({ channel, component, id: "native:1", result: {} }));
  assert.equal(deliveries, 0);
  assert.equal(f.sent.length, 0);
  assert.equal(f.scripts.length, 1);
  assert.throws(() => f.context.BKCatNative.postMessage(JSON.stringify({ channel, event: "runtime.loaded" })), /retired/);
});
