// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { ExecutionGate } from "../src/gate.ts";
import { NativeMcpTransport } from "../src/native-adapters.ts";
import type { InvocationContext, NativePorts, ToolApproval } from "../src/ports.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

const original = { conversationId: 5, taskId: 7, callId: "original-edit" };
const argumentsForEdit = () => ({ path: "test-project", expectedSession: 3, expectedRevision: 0,
  command: { op: "addTextClip", trackId: "T3", start: 1, duration: 2, style: { content: "审批测试" } } });

async function fixture(authorize?: NativePorts["mcp"]["authorize"]) {
  const lease = { storageId: "approval-test", generation: "1", handle: "native-owner" };
  const gate = new ExecutionGate(); gate.acquiring(); gate.owned(lease);
  gate.activating(lease, { generation: "1", snapshotId: "reviewed", allowInitializeEmptyStore: false }); gate.ready(lease);
  const received: { message: any; invocation?: InvocationContext; ticket?: string }[] = [];
  const errors: Error[] = [];
  const ports = { mcp: {
    authorize: authorize ?? (async (_lease, _message, _signal, invocation) => ({ approved: true, ticket: `native-ticket-${invocation.callId}` })),
    async send(_lease, message, _signal, invocation, ticket) { received.push({ message, invocation, ticket }); return undefined; },
  } } as NativePorts;
  const transport = new NativeMcpTransport(ports, lease, gate);
  transport.onError((error) => { errors.push(error); });
  await transport.start();
  const send = (args: Record<string, unknown>) => transport.send({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "edit_apply_guarded", arguments: args } });
  const execute = (args = argumentsForEdit(), invocation = original, signal = new AbortController().signal) =>
    transport.withInvocation(args, invocation, async () => {
      const decision = await transport.authorizeTool(args, signal);
      if (!decision.approved) return { isError: true, message: decision.message };
      await send(args);
      return { isError: false };
    });
  return { transport, gate, received, errors, send, execute };
}

test("approval wait dispatches no protocol tool request and retains the same invocation when allowed", async () => {
  const decision = deferred<ToolApproval>();
  let approvalFrame: any;
  const f = await fixture(async (_lease, message, _signal, invocation) => {
    approvalFrame = { message, invocation }; return decision.promise;
  });
  try {
    const args = argumentsForEdit();
    const pending = f.execute(args);
    assert.deepEqual(approvalFrame.invocation, original);
    assert.equal(approvalFrame.message.id, undefined, "approval is not a timed MCP request");
    assert.equal(approvalFrame.message.params.arguments, args);
    assert.equal(f.received.length, 0);
    decision.resolve({ approved: true, ticket: "one-native-ticket" });
    assert.equal((await pending).isError, false);
    assert.equal(f.received.length, 1);
    assert.deepEqual(f.received[0]!.invocation, original);
    assert.equal(f.received[0]!.ticket, "one-native-ticket");
    assert.equal("approvalTicket" in f.received[0]!.message.params.arguments, false);
    assert.equal("invocation" in f.received[0]!.message.params.arguments, false);
  } finally { await f.transport.close(); }
});

test("refusal and approval expiry are ordinary tool outcomes, without dispatch or transport failure", async () => {
  for (const message of ["用户拒绝了本次剪辑操作，工程未修改。", "等待剪辑批准超时，工程未修改。"] ) {
    const f = await fixture(async () => ({ approved: false, message }));
    try {
      assert.deepEqual(await f.execute(), { isError: true, message });
      assert.equal(f.received.length, 0);
      assert.equal(f.errors.length, 0);
      assert.equal(f.gate.phase, "ready");
    } finally { await f.transport.close(); }
  }
});

test("task cancel, owner revoke and transport close reject late approvals without executing", async () => {
  for (const reason of ["task", "owner", "close"]) {
    const decision = deferred<ToolApproval>();
    let signal!: AbortSignal;
    const f = await fixture(async (_lease, _message, supplied) => { signal = supplied; return decision.promise; });
    const abort = new AbortController();
    try {
      const pending = f.execute(argumentsForEdit(), original, abort.signal);
      if (reason === "task") abort.abort();
      else if (reason === "owner") f.gate.stop();
      else await f.transport.close();
      if (reason !== "close") assert.equal(signal.aborted, true);
      decision.resolve({ approved: true, ticket: "late-native-ticket" });
      await assert.rejects(pending);
      assert.equal(f.received.length, 0);
    } finally { await f.transport.close(); }
  }
});

test("pre-cancelled execution does not even ask for approval", async () => {
  let asked = 0;
  const f = await fixture(async () => { asked++; return { approved: true, ticket: "never" }; });
  const abort = new AbortController(); abort.abort();
  try {
    await assert.rejects(f.execute(argumentsForEdit(), original, abort.signal));
    assert.equal(asked, 0); assert.equal(f.received.length, 0);
  } finally { await f.transport.close(); }
});

test("duplicate invocation and approval tickets cannot dispatch a second mutation", async () => {
  const f = await fixture(async () => ({ approved: true, ticket: "single-native-ticket" }));
  try {
    assert.equal((await f.execute()).isError, false);
    await assert.rejects(f.execute(), /already requested/);
    await assert.rejects(f.execute(argumentsForEdit(), { ...original, callId: "different-call" }), /ticket was already issued/);
    assert.equal(f.received.length, 1);
  } finally { await f.transport.close(); }
});

test("concurrent argument copies cannot reuse one original invocation", async () => {
  const decision = deferred<ToolApproval>();
  const f = await fixture(async () => decision.promise);
  try {
    const first = f.execute();
    await assert.rejects(f.execute(), /already active/);
    decision.resolve({ approved: false, message: "用户拒绝" }); await first;
    assert.equal(f.received.length, 0);
  } finally { await f.transport.close(); }
});

test("the approved ticket is consumed before dispatch and model JSON cannot supply one", async () => {
  const f = await fixture();
  const args = { ...argumentsForEdit(), approvalTicket: "model-forged-ticket" };
  try {
    await f.transport.withInvocation(args, original, async () => {
      await assert.rejects(f.send(args), /unused native approval/);
      await f.transport.authorizeTool(args, new AbortController().signal);
      await f.send(args);
      await assert.rejects(f.send(args), /unused native approval/);
    });
    assert.equal(f.received.length, 1);
    assert.equal(f.received[0]!.ticket, "native-ticket-original-edit");
  } finally { await f.transport.close(); }
});

test("changing the command during approval or before dispatch fails closed", async () => {
  for (const timing of ["during", "after"]) {
    const decision = deferred<ToolApproval>();
    const f = await fixture(async () => decision.promise);
    const args = argumentsForEdit();
    try {
      const pending = f.transport.withInvocation(args, original, async () => {
        const approval = f.transport.authorizeTool(args, new AbortController().signal);
        if (timing === "during") args.command.duration = 9;
        decision.resolve({ approved: true, ticket: "native-ticket" });
        await approval;
        if (timing === "after") args.command.duration = 9;
        await f.send(args);
      });
      await assert.rejects(pending, /arguments changed/);
      assert.equal(f.received.length, 0);
    } finally { await f.transport.close(); }
  }
});
