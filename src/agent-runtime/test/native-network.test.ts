// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { NativeWireClient, messageNativePorts, NATIVE_CHANNEL, type NativeMessageBridge } from "../src/native-wire.ts";

const lease = { storageId: "isolated-stream", handle: "stream-owner", generation: "1" };
function fixture(respond: (frame: any) => unknown | Promise<unknown>) {
  const sent: any[] = [];
  const listeners = new Set<(event: { data: string }) => void>();
  const receive = (message: unknown) => { for (const listener of listeners) listener({ data: JSON.stringify(message) }); };
  const bridge: NativeMessageBridge = {
    postMessage(value) {
      const frame = JSON.parse(value); sent.push(frame);
      if (!frame.id) return;
      void Promise.resolve().then(() => respond(frame)).then(
        (result) => receive({ channel: NATIVE_CHANNEL, id: frame.id, result }),
        () => receive({ channel: NATIVE_CHANNEL, id: frame.id, error: { message: "native fixed error" } }),
      );
    },
    addEventListener(_type, listener) { listeners.add(listener); },
    removeEventListener(_type, listener) { listeners.delete(listener); },
  };
  const wire = new NativeWireClient(bridge); wire.bindOwner(lease);
  return { wire, sent, receive, ports: messageNativePorts(wire) };
}

test("native stream preserves UTF-8 across chunks, status and allowlisted headers without sending credentials", async () => {
  const bytes = new TextEncoder().encode("data: 中文\n\n");
  let reads = 0;
  const f = fixture((frame) => {
    if (frame.payload.method === "network.open") return { streamId: "s", status: 200, headers: [["Content-Type", "text/event-stream"], ["set-cookie", "never-keep"]] };
    assert.equal(frame.payload.method, "network.read");
    const part = reads++ === 0 ? bytes.slice(0, 7) : bytes.slice(7);
    return { chunk: Buffer.from(part).toString("base64"), done: reads === 2 };
  });
  try {
    const response = await f.ports.network.fetch(lease, "task-permit", "https://model.invalid/v1/chat/completions", {
      method: "POST", body: "{}", headers: { Authorization: "Bearer private-must-never-cross", "x-api-key": "private-key", cookie: "private-cookie", "content-type": "application/json" },
    });
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("content-type"), "text/event-stream");
    assert.equal(response.headers.has("set-cookie"), false);
    assert.equal(await response.text(), "data: 中文\n\n");
    const open = f.sent[0].payload;
    assert.equal(open.methodName, "POST");
    assert.deepEqual(open.headers, [["content-type", "application/json"]]);
    assert.equal(JSON.stringify(f.sent).includes("private-"), false);
    assert.equal(f.sent.filter((frame) => frame.payload.method === "network.read").length, 2);
  } finally { f.wire.close(); }
});

test("abort after native open cancels the owned stream and rejects late content", async () => {
  let release!: (value: unknown) => void;
  const reading = new Promise((resolve) => { release = resolve; });
  const f = fixture((frame) => frame.payload.method === "network.open"
    ? { streamId: "s", status: 200, headers: [] }
    : frame.payload.method === "network.read" ? reading : null);
  const abort = new AbortController();
  try {
    const response = await f.ports.network.fetch(lease, "task-permit", "https://model.invalid/v1/chat/completions", { method: "POST", body: "{}", signal: abort.signal });
    const text = response.text();
    abort.abort();
    await assert.rejects(text, /cancelled/);
    release({ chunk: Buffer.from("late").toString("base64"), done: true });
    await Promise.resolve();
    assert.ok(f.sent.some((frame) => frame.payload.method === "network.cancel" && frame.payload.streamId === "s" && frame.payload.networkPermit === "task-permit"));
  } finally { release({ chunk: "", done: true }); f.wire.close(); }
});

test("ownership revocation and channel close fail a stream even while between native read replies", async () => {
  for (const stop of ["revoke", "close"]) {
    const f = fixture((frame) => frame.payload.method === "network.open"
      ? { streamId: "s", status: 200, headers: [] }
      : frame.payload.method === "network.read" ? new Promise(() => {}) : null);
    const response = await f.ports.network.fetch(lease, "task-permit", "https://model.invalid/v1/chat/completions", { method: "POST", body: "{}" });
    const text = response.text();
    if (stop === "revoke") f.receive({ channel: NATIVE_CHANNEL, event: "owner.revoked", ownerHandle: lease.handle, generation: lease.generation });
    else f.wire.close();
    await assert.rejects(text, /cancelled/);
    f.wire.close();
  }
});

test("oversize and malformed native chunks terminate without publishing partial success", async () => {
  for (const chunk of ["not base64", Buffer.alloc(65537).toString("base64"), ""]) {
    const f = fixture((frame) => frame.payload.method === "network.open"
      ? { streamId: "s", status: 200, headers: [] }
      : frame.payload.method === "network.read" ? { chunk, done: false } : null);
    try {
      const response = await f.ports.network.fetch(lease, "task-permit", "https://model.invalid/v1/chat/completions", { method: "POST", body: "{}" });
      await assert.rejects(response.text(), /failed/);
      assert.ok(f.sent.some((frame) => frame.payload.method === "network.cancel"));
    } finally { f.wire.close(); }
  }
});

test("a request cancelled while opening releases the late native stream", async () => {
  let release!: (value: unknown) => void;
  const open = new Promise((resolve) => { release = resolve; });
  const f = fixture((frame) => frame.payload.method === "network.open" ? open : null);
  const abort = new AbortController();
  const request = f.ports.network.fetch(lease, "task-permit", "https://model.invalid/v1/chat/completions", { method: "POST", body: "{}", signal: abort.signal });
  try {
    while (!f.sent.some((frame) => frame.payload.method === "network.open")) await new Promise((resolve) => setImmediate(resolve));
    abort.abort();
    release({ streamId: "late-stream", status: 200, headers: [] });
    await assert.rejects(request);
    assert.ok(f.sent.some((frame) => frame.payload.method === "network.cancel" && frame.payload.streamId === "late-stream" && frame.payload.networkPermit === "task-permit"));
  } finally { f.wire.close(); }
});
