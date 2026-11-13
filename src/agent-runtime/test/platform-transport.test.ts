// SPDX-License-Identifier: AGPL-3.0-or-later
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const component = "a".repeat(64);
const channel = "bk-cat-runtime-v1";
const source = readFileSync(new URL("../../crates/concat/src/embedded/transport.js", import.meta.url), "utf8")
  .replace("__BKCAT_COMPONENT__", JSON.stringify(component));

function desktop() {
  const sent: string[] = [];
  let receive: (event: { data: string }) => void = () => {};
  const context = vm.createContext({
    chrome: { webview: {
      postMessage(value: string) { sent.push(value); },
      addEventListener(_type: string, listener: typeof receive) { receive = listener; },
    } },
  });
  vm.runInContext(source, context);
  return { context, sent, receive: (data: string) => receive({ data }) };
}

test("desktop transport binds every original-runtime frame to its native component", () => {
  const { context, sent } = desktop();
  context.BKCatNative.postMessage(JSON.stringify({
    channel, id: "js:1", component: "forged", payload: { method: "recovery.read" },
  }));
  assert.equal(sent.length, 1);
  const frame = JSON.parse(sent[0]!);
  assert.equal(frame.component, component);
  assert.equal(frame.id, "js:1");
  assert.equal(frame.payload.method, "recovery.read");
  assert.throws(() => vm.runInContext('"use strict"; BKCatNative = {};', context));
  assert.throws(() => vm.runInContext('"use strict"; __BKCatDeliver = () => {};', context));
});

test("stale and wrong-channel native replies cannot reach Pi listeners", () => {
  const { context, receive } = desktop();
  const replies: string[] = [];
  const listener = (event: { data: string }) => replies.push(event.data);
  context.BKCatNative.addEventListener("message", listener);
  receive(JSON.stringify({ channel, component: "old", id: "js:1", result: {} }));
  receive(JSON.stringify({ channel: "other", component, id: "js:1", result: {} }));
  receive("invalid");
  assert.equal(replies.length, 0);
  receive(JSON.stringify({ channel, component, id: "js:1", result: { actual: true } }));
  assert.equal(replies.length, 1);
  const reply = JSON.parse(replies[0]!);
  assert.equal(reply.result.actual, true);
  assert.equal("component" in reply, false);
  context.BKCatNative.removeEventListener("message", listener);
  receive(JSON.stringify({ channel, component, id: "js:2", result: {} }));
  assert.equal(replies.length, 1);
});

test("Android transport uses only the native bridge and the same fenced inbox", () => {
  const sent: string[] = [];
  const context = vm.createContext({ BKCatAndroid: { postMessage(value: string) { sent.push(value); } } });
  vm.runInContext(source, context);
  context.BKCatNative.postMessage(JSON.stringify({ channel, event: "runtime.loaded" }));
  assert.equal(JSON.parse(sent[0]!).component, component);
  let count = 0;
  context.BKCatNative.addEventListener("message", () => count++);
  context.__BKCatDeliver(JSON.stringify({ channel, component: "retired", id: "native:1", payload: {} }));
  assert.equal(count, 0);
  context.__BKCatDeliver(JSON.stringify({ channel, component, id: "native:1", payload: {} }));
  assert.equal(count, 1);
});

test("absent transport, malformed frames and resource exhaustion fail closed", () => {
  assert.throws(() => vm.runInNewContext(source, {}));
  const { context, sent } = desktop();
  for (const value of ["null", "[]", '{"channel":"other"}', "x".repeat(2 * 1024 * 1024 + 1)]) {
    assert.throws(() => context.BKCatNative.postMessage(value));
  }
  assert.equal(sent.length, 0);
  for (let i = 0; i < 32; i++) context.BKCatNative.addEventListener("message", () => {});
  assert.throws(() => context.BKCatNative.addEventListener("message", () => {}));
});
