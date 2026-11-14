// SPDX-License-Identifier: AGPL-3.0-or-later

import test from "node:test";
import assert from "node:assert/strict";
import { AgentRuntime } from "../src/runtime.ts";
import { ExecutionGate } from "../src/gate.ts";
import { guardedDatabase, guardedStorage, NativeMcpTransport } from "../src/native-adapters.ts";
import type { NativePorts, OwnerLease, PiEngine, PiEngineFactory, SchedulingAction } from "../src/ports.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

function fixture() {
  const log: string[] = [];
  const lease: OwnerLease = { storageId: "store", generation: "1", handle: "native-capability" };
  const snapshot = { snapshotId: "snapshot", projectSession: "7", projectRevision: "41", sessionRevision: "9", schemaVersion: 2, hasInterruptedWork: false };
  let listener: (() => void) | undefined;
  let held = false;
  const ports: NativePorts = {
    ownership: {
      async acquire() { if (held) throw new Error("native owner busy"); held = true; log.push("acquire"); return lease; },
      async confirm() { log.push("confirm"); return { generation: lease.generation, snapshotId: snapshot.snapshotId, allowInitializeEmptyStore: false }; },
      async revoke() { log.push("revoke"); listener?.(); },
      async drainAndRelease() { log.push("release"); held = false; },
      onRevoked(_lease, callback) { listener = callback; return () => { listener = undefined; }; },
    },
    recovery: { async read() { log.push("read-recovery"); return { ...snapshot }; } },
    sqlite: { async open() { throw new Error("SQLite was not expected in lifecycle-port tests"); } },
    mcp: { async send() { log.push("mcp"); return undefined; } },
    network: { async fetch() { log.push("network"); throw new Error("network must not run in this test"); } },
  };
  const engine: PiEngine = {
    async inspect() { log.push("inspect"); return {}; },
    async observeConversation() { return { value: {}, detach() {} }; },
    async run(action) { log.push(`run:${action}`); return {}; },
    async close() { log.push("engine-close"); },
  };
  const factory: PiEngineFactory = { async open() { log.push("engine-open"); return engine; } };
  const runtime = new AgentRuntime("store", ports, factory);
  const approval = { snapshotId: "snapshot", decision: "continue" as const, approvalId: "native-user-action" };
  return { log, lease, snapshot, ports, engine, factory, runtime, approval };
}

const actions: SchedulingAction[] = [
  "createConversation", "submit", "waitSubmission", "abortSubmission", "resume", "abortTask", "signal",
  "waitForTask", "waitForIdle", "compact", "abortConversation", "resetConversation", "nativeSubmit", "nativeWait",
];

test("startup only acquires ownership and reads recovery; every scheduling entry stays closed", async () => {
  const f = fixture();
  await f.runtime.start();
  assert.equal(f.runtime.phase, "recovery");
  for (const action of actions) await assert.rejects(f.runtime.dispatch(action), /reconciliation/);
  assert.deepEqual(f.log, ["acquire", "read-recovery"]);
  await f.runtime.stop();
  assert.deepEqual(f.log, ["acquire", "read-recovery", "revoke", "release"]);
});

test("confirmation opens Pi once but does not resume or call a model", async () => {
  const f = fixture();
  await f.runtime.start();
  await f.runtime.confirmRecovery(f.approval);
  assert.equal(f.runtime.phase, "ready");
  assert.deepEqual(f.log, ["acquire", "read-recovery", "confirm", "engine-open"]);
  await f.runtime.dispatch("submit", {});
  assert.equal(f.log.at(-1), "run:submit");
  await f.runtime.stop();
  assert.deepEqual(f.log.slice(-3), ["revoke", "engine-close", "release"]);
});

test("a recovery grant alone is not network permission and task permissions never survive completion or stop", () => {
  const f = fixture();
  const gate = new ExecutionGate(); gate.acquiring(); gate.owned(f.lease);
  gate.activating(f.lease, { generation: "1", snapshotId: "snapshot", allowInitializeEmptyStore: false }); gate.ready(f.lease);
  assert.throws(() => gate.networkPermit(f.lease), /no approved/);
  gate.authorizeNetwork(f.lease, "task-one"); assert.equal(gate.networkPermit(f.lease), "task-one");
  assert.throws(() => gate.authorizeNetwork(f.lease, "task-two"), /another task/);
  gate.finishNetwork("wrong-task"); assert.equal(gate.networkPermit(f.lease), "task-one");
  gate.finishNetwork("task-one"); assert.throws(() => gate.networkPermit(f.lease), /no approved/);
  gate.authorizeNetwork(f.lease, "task-two"); gate.stop();
  assert.throws(() => gate.networkPermit(f.lease)); assert.throws(() => gate.authorizeNetwork(f.lease, "task-three"));
});

test("stale approval and discard with interrupted work never open writable Pi", async () => {
  const f = fixture();
  f.snapshot.hasInterruptedWork = true;
  await f.runtime.start();
  await assert.rejects(f.runtime.confirmRecovery({ ...f.approval, snapshotId: "old" }), /snapshot/);
  await assert.rejects(f.runtime.confirmRecovery({ ...f.approval, decision: "discard" }), /discard reconciliation/);
  assert.deepEqual(f.log, ["acquire", "read-recovery"]);
  await f.runtime.stop();
});

test("overlapping recovery confirmation has one native grant and one Pi open", async () => {
  const f = fixture();
  const grant = deferred<Awaited<ReturnType<NativePorts["ownership"]["confirm"]>>>();
  f.ports.ownership.confirm = async () => { f.log.push("confirm"); return grant.promise; };
  await f.runtime.start();
  const first = f.runtime.confirmRecovery(f.approval);
  await assert.rejects(f.runtime.confirmRecovery(f.approval), /not awaiting/);
  grant.resolve({ generation: "1", snapshotId: "snapshot", allowInitializeEmptyStore: false });
  await first;
  assert.equal(f.log.filter((item) => item === "engine-open").length, 1);
  await f.runtime.stop();
});

test("stop while acquire is pending revokes the eventual lease before release", async () => {
  const f = fixture();
  const acquired = deferred<OwnerLease>();
  f.ports.ownership.acquire = async () => acquired.promise;
  const starting = f.runtime.start();
  const stopping = f.runtime.stop();
  acquired.resolve(f.lease);
  await assert.rejects(starting, /stopped while acquiring/);
  await stopping;
  assert.deepEqual(f.log, ["revoke", "release"]);
  assert.equal(f.runtime.phase, "closed");
});

test("stop during Pi open retains ownership until the late engine is closed", async () => {
  const f = fixture();
  const opening = deferred<PiEngine>();
  f.factory.open = async () => { f.log.push("engine-open"); return opening.promise; };
  await f.runtime.start();
  const activating = f.runtime.confirmRecovery(f.approval);
  await Promise.resolve();
  const stopping = f.runtime.stop();
  assert.equal(f.log.at(-1), "revoke");
  assert(!f.log.includes("release"));
  opening.resolve(f.engine);
  await assert.rejects(activating, /no longer active/);
  await stopping;
  assert.deepEqual(f.log.slice(-3), ["revoke", "engine-close", "release"]);
});

test("a replacement host starts in recovery after the prior owner drains", async () => {
  const f = fixture();
  await f.runtime.start();
  const overlapping = new AgentRuntime("store", f.ports, f.factory);
  await assert.rejects(overlapping.start(), /busy/);
  await f.runtime.confirmRecovery(f.approval);
  await f.runtime.stop();
  const replacement = new AgentRuntime("store", f.ports, f.factory);
  await replacement.start();
  assert.equal(replacement.phase, "recovery");
  await assert.rejects(replacement.dispatch("waitForIdle"), /reconciliation/);
  assert.equal(f.log.filter((item) => item === "engine-open").length, 1);
  await replacement.stop();
});

test("late results cannot authorize a new action after stop", async () => {
  const f = fixture();
  const result = deferred<unknown>();
  f.engine.run = async () => { f.log.push("run"); return result.promise; };
  await f.runtime.start();
  await f.runtime.confirmRecovery(f.approval);
  const running = f.runtime.dispatch("submit", {});
  await f.runtime.stop();
  result.resolve({ old: true });
  await assert.rejects(running, /no longer active/);
  await assert.rejects(f.runtime.dispatch("resume"), /no longer active/);
  assert.equal(f.log.filter((item) => item === "run").length, 1);
});

test("real-view observer callback from an old generation is not delivered", async () => {
  const f = fixture();
  let callback: Parameters<PiEngine["observeConversation"]>[1] | undefined;
  let detached = 0;
  let delivered = 0;
  f.engine.observeConversation = async (_id, listener) => {
    callback = listener;
    return { value: { entries: [] }, detach() { detached++; } };
  };
  await f.runtime.start();
  await f.runtime.confirmRecovery(f.approval);
  const observation = await f.runtime.observeConversation(2, () => { delivered++; });
  assert.deepEqual(observation.value, { entries: [] });
  callback!({ kind: "value", value: { entries: ["actual source update"] } });
  assert.equal(delivered, 1);
  await f.runtime.stop();
  callback!({ kind: "value", value: { entries: ["late source update"] } });
  assert.equal(delivered, 1);
  assert.equal(detached, 1);
});

test("failed Pi open revokes and releases the owned generation", async () => {
  const f = fixture();
  f.factory.open = async () => { throw new Error("open failed"); };
  await f.runtime.start();
  await assert.rejects(f.runtime.confirmRecovery(f.approval), /open failed/);
  assert.equal(f.runtime.phase, "closed");
  assert.deepEqual(f.log.slice(-2), ["revoke", "release"]);
});

test("an observer cleanup failure cannot prevent native revocation, engine cleanup, or ownership drain", async () => {
  const f = fixture();
  f.engine.observeConversation = async () => ({ value: {}, detach() { throw new Error("listener cleanup failed"); } });
  await f.runtime.start(); await f.runtime.confirmRecovery(f.approval);
  await f.runtime.observeConversation(2, () => {});
  await assert.rejects(f.runtime.stop(), AggregateError);
  assert.deepEqual(f.log.slice(-3), ["revoke", "engine-close", "release"]);
  assert.equal(f.runtime.phase, "closed");
});

function openedGate(ready = true) {
  const f = fixture();
  const gate = new ExecutionGate();
  gate.acquiring();
  gate.owned(f.lease);
  if (ready) {
    gate.activating(f.lease, { generation: "1", snapshotId: "snapshot", allowInitializeEmptyStore: false });
    gate.ready(f.lease);
  }
  return { ...f, gate };
}

test("a generation without a model budget cannot obtain a network permit", () => {
  const f = openedGate();
  assert.throws(() => f.gate.networkPermit(f.lease), /budget/);
  assert.throws(() => f.gate.assertReady({ ...f.lease, generation: "2" }), /no longer active/);
});

test("SQLite transaction callback that loses ownership cannot reach commit", async () => {
  const f = openedGate();
  let committed = false;
  let rolledBack = false;
  const db = {
    async exec() {}, async run() {}, async runCounted() { return 0; },
    async get() { return undefined; }, async all() { return []; }, async close() {},
    async transaction<T>(callback: (tx: typeof db) => Promise<T>): Promise<T> {
      try { const result = await callback(db); committed = true; return result; }
      catch (error) { rolledBack = true; throw error; }
    },
  };
  const guarded = guardedDatabase(db, f.gate, f.lease);
  await assert.rejects(guarded.transaction(async (tx) => {
    await tx.run("INSERT INTO test VALUES (?)", 1);
    f.gate.stop();
  }), /no longer active/);
  assert.equal(committed, false);
  assert.equal(rolledBack, true);
  await assert.rejects(guarded.run("INSERT INTO test VALUES (?)", 2), /no longer active/);
});

test("late SQLite durability receipt cannot publish through an old generation", async () => {
  const f = openedGate();
  const committed = deferred<void>();
  let published = 0;
  const raw = {
    async commit(_batches: unknown, receive: (index: number, seq: number) => void) {
      await committed.promise;
      receive(0, 1);
      return { stored: 1 };
    },
  };
  const storage = guardedStorage(raw as never, f.gate, f.lease);
  const pending = storage.commit([[]], () => { published++; }, {} as never);
  f.gate.stop();
  committed.resolve();
  await assert.rejects(pending, /no longer active/);
  assert.equal(published, 0);
});

test("MCP transport blocks recovery edits, hidden tools, and cancelled sends before native dispatch", async () => {
  const f = openedGate(false);
  const transport = new NativeMcpTransport(f.ports, f.lease, f.gate);
  await transport.start();
  await assert.rejects(transport.send({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "edit_apply_guarded" } }), /reconciliation/);
  await assert.rejects(transport.send({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "edit_apply" } }), /unreviewed/);
  const abort = new AbortController();
  abort.abort();
  await assert.rejects(transport.send({ jsonrpc: "2.0", id: 3, method: "tools/list" }, { signal: abort.signal }));
  assert(!f.log.includes("mcp"));
  await transport.close();
});

test("MCP late reply is discarded when native ownership has been revoked", async () => {
  const f = openedGate();
  const reply = deferred<unknown>();
  f.ports.mcp.send = async () => reply.promise;
  const transport = new NativeMcpTransport(f.ports, f.lease, f.gate);
  let delivered = 0;
  transport.onMessage(() => { delivered++; });
  await transport.start();
  const pending = transport.send({ jsonrpc: "2.0", id: 1, method: "tools/list" });
  f.gate.stop();
  reply.resolve({ jsonrpc: "2.0", id: 1, result: {} });
  await assert.rejects(pending, /no longer active/);
  assert.equal(delivered, 0);
  await transport.close();
});

test("guarded MCP edits take operation identity only from the original tool context, not model JSON", async () => {
  const f = openedGate();
  const received: unknown[] = [];
  f.ports.mcp.send = async (_lease, _message, _signal, invocation) => { received.push(invocation); return undefined; };
  const transport = new NativeMcpTransport(f.ports, f.lease, f.gate);
  await transport.start();
  const args = { expectedSession: 1, expectedRevision: 0, invocation: { taskId: 999, conversationId: 999, callId: "forged" } };
  const message = { jsonrpc: "2.0" as const, id: 1, method: "tools/call", params: { name: "edit_apply_guarded", arguments: args } };
  await assert.rejects(transport.send(message), /original Pi invocation/);
  assert.equal(received.length, 0);
  await transport.withInvocation(args, { taskId: 7, conversationId: 8, callId: "original" }, () => transport.send(message));
  assert.deepEqual(received, [{ taskId: 7, conversationId: 8, callId: "original" }]);
  await assert.rejects(transport.send(message), /original Pi invocation/);
  assert.equal(received.length, 1);
  await transport.close();
});

test("concurrent MCP calls keep distinct trusted invocation identities", async () => {
  const f = openedGate();
  const received: unknown[] = [];
  const pending = deferred<void>();
  f.ports.mcp.send = async (_lease, _message, _signal, invocation) => { received.push(invocation); await pending.promise; return undefined; };
  const transport = new NativeMcpTransport(f.ports, f.lease, f.gate);
  await transport.start();
  const args1 = {}; const args2 = {};
  const call = (id: number, args: Record<string, unknown>) => transport.withInvocation(args,
    { taskId: id, conversationId: id + 10, callId: `call-${id}` },
    () => transport.send({ jsonrpc: "2.0", id, method: "tools/call", params: { name: "edit_apply_guarded", arguments: args } }));
  const first = call(1, args1); const second = call(2, args2);
  assert.deepEqual(received, [{ taskId: 1, conversationId: 11, callId: "call-1" }, { taskId: 2, conversationId: 12, callId: "call-2" }]);
  pending.resolve(); await Promise.all([first, second]);
  await transport.close();
});
