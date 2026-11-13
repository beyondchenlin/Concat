// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { NativeWireClient, messageNativePorts, NATIVE_CHANNEL, type NativeMessageBridge } from "../src/native-wire.ts";

function bridge() {
  const sent: Record<string, any>[] = [];
  const listeners = new Set<(event: { data: string }) => void>();
  const api: NativeMessageBridge = {
    postMessage(value) { sent.push(JSON.parse(value)); },
    addEventListener(_type, listener) { listeners.add(listener); },
    removeEventListener(_type, listener) { listeners.delete(listener); },
  };
  return { api, sent, receive(value: unknown) { for (const listener of listeners) listener({ data: JSON.stringify(value) }); } };
}

test("native message correlation ignores wrong channels and duplicate/late replies without resending", async () => {
  const b = bridge(); const wire = new NativeWireClient(b.api);
  const first = wire.call({ method: "recovery.read", ownerHandle: "one", generation: "3" });
  const id = b.sent[0]!.id;
  let settled = false; void first.then(() => { settled = true; });
  b.receive({ channel: "other", id, result: "wrong" });
  await Promise.resolve(); assert.equal(settled, false);
  b.receive({ channel: NATIVE_CHANNEL, id, result: "correct" });
  assert.equal(await first, "correct");
  b.receive({ channel: NATIVE_CHANNEL, id, result: "late" });
  assert.equal(b.sent.length, 1);
  wire.close();
  await assert.rejects(wire.call({ method: "late" }));
});

test("abort sends a native cancellation but preserves the terminal resource response for cleanup", async () => {
  const b = bridge(); const wire = new NativeWireClient(b.api); const abort = new AbortController();
  const open = wire.call({ method: "sqlite.open", ownerHandle: "one", generation: "3" }, abort.signal);
  const id = b.sent[0]!.id;
  abort.abort();
  assert.equal(b.sent[1]!.payload.method, "request.cancel");
  assert.equal(b.sent[1]!.payload.requestId, id);
  b.receive({ channel: NATIVE_CHANNEL, id, result: { databaseId: "late-owned-resource" } });
  assert.deepEqual(await open, { databaseId: "late-owned-resource" });
  wire.close();
});

test("revocation delivered between acquire reply and subscription cannot be missed", () => {
  const b = bridge(); const wire = new NativeWireClient(b.api);
  b.receive({ channel: NATIVE_CHANNEL, event: "owner.revoked", ownerHandle: "one", generation: "3" });
  let matching = 0; let other = 0;
  wire.onRevoked({ storageId: "x", handle: "one", generation: "3" }, () => { matching++; });
  wire.onRevoked({ storageId: "x", handle: "one", generation: "4" }, () => { other++; });
  assert.equal(matching, 1); assert.equal(other, 0);
  wire.close();
});

test("saturated native channel still admits revoke and cleanup; close rejects all outstanding work", async () => {
  const b = bridge(); const wire = new NativeWireClient(b.api);
  const pending = Array.from({ length: 1024 }, () => wire.call({ method: "recovery.read" }).catch(() => "closed"));
  await assert.rejects(wire.call({ method: "mcp.send" }), /limit/);
  const cleanup = wire.call({ method: "ownership.revoke", ownerHandle: "one", generation: "3" });
  const id = b.sent.at(-1)!.id;
  b.receive({ channel: NATIVE_CHANNEL, id, result: null }); await cleanup;
  wire.close();
  assert.equal((await Promise.all(pending)).every((item) => item === "closed"), true);
});

test("native ports bind the trusted invocation beside MCP JSON, and never fall back to browser fetch", async () => {
  const b = bridge(); const wire = new NativeWireClient(b.api); const ports = messageNativePorts(wire);
  const invocation = { taskId: 7, conversationId: 8, callId: "original-call" };
  const call = ports.mcp.send({ storageId: "x", handle: "owner", generation: "4" }, { jsonrpc: "2.0", id: 1, method: "tools/call" }, new AbortController().signal, invocation);
  const frame = b.sent.at(-1)!;
  assert.deepEqual(frame.payload.invocation, invocation);
  assert.equal(frame.payload.ownerHandle, "owner"); assert.equal(frame.payload.generation, "4");
  b.receive({ channel: NATIVE_CHANNEL, id: frame.id, result: null }); assert.equal(await call, undefined);
  await assert.rejects(ports.network.fetch({ storageId: "x", handle: "owner", generation: "4" }, "budget", "https://not-configured.invalid", {}));
  assert.equal(b.sent.length, 1);
  wire.close();
});
