// SPDX-License-Identifier: AGPL-3.0-or-later

// This test executes the built original Pi components with a real isolated
// SQLite file using Node's SQLite driver. Native ownership and MCP are
// explicit fixtures here; it is not a Rust/WebView/device/model acceptance.
import assert from "node:assert/strict";
import { createHash, webcrypto } from "node:crypto";
import { copyFileSync, existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { pathToFileURL } from "node:url";
import test from "node:test";
import vm from "node:vm";
import { packageRoot, pins, referencePaths, referenceRoot, verifyReference } from "../scripts/reference.mjs";

const require = verifyReference();
const bundle = readFileSync(resolve(packageRoot, "dist/runtime.js"));
const manifest = JSON.parse(readFileSync(resolve(packageRoot, "dist/manifest.json"), "utf8"));
assert.equal(manifest.piCommit, pins.commit);
assert.equal(createHash("sha256").update(bundle).digest("hex"), manifest.sha256);
for (const [name, expected] of Object.entries(manifest.files)) {
  const file = readFileSync(resolve(packageRoot, "dist", name));
  assert.equal(file.length, expected.bytes);
  assert.equal(createHash("sha256").update(file).digest("hex"), expected.sha256);
}
let forbiddenFetch = 0;
const sandbox = vm.createContext({
  AbortController, AbortSignal, DOMException, URL, URLSearchParams,
  // These are real standard web implementations, not model/network stubs.
  // The pinned SDK checks FormData even when encoding a plain JSON request;
  // Node's VM does not expose its host's web globals automatically.
  Request, Response, Headers, FormData, Blob, File,
  ReadableStream, TextDecoder, TextEncoder, Uint8Array,
  setTimeout, clearTimeout, queueMicrotask, performance, crypto: webcrypto, btoa, atob,
  fetch: () => { forbiddenFetch++; throw new Error("Direct browser network is forbidden in this test"); },
});
// Node's native structuredClone returns host-realm plain objects. A real
// browser returns objects in the caller's web realm; original Pi's strict JSON
// admission checks that prototype identity. Keep native cloning (including
// rejection, aliasing and cycles) and transfer only plain-object/array graphs
// into the isolated browser realm before original tool validation consumes it.
const webPrototypes = vm.runInContext("({ object: Object.prototype, array: Array.prototype })", sandbox);
const webJsonParse = vm.runInContext("JSON.parse", sandbox);
const nativeJsonReply = (value) => webJsonParse(JSON.stringify(value));
sandbox.structuredClone = (value, options) => {
  const cloned = structuredClone(value, options);
  const seen = new Set();
  const adoptPlain = (item) => {
    if (!item || typeof item !== "object" || seen.has(item)) return;
    seen.add(item);
    const prototype = Object.getPrototypeOf(item);
    if (prototype !== Object.prototype && prototype !== Array.prototype) return;
    for (const key of Object.keys(item)) adoptPlain(item[key]);
    Object.setPrototypeOf(item, prototype === Array.prototype ? webPrototypes.array : webPrototypes.object);
  };
  adoptPlain(cloned);
  return cloned;
};
vm.runInContext(bundle.toString("utf8"), sandbox, { filename: "runtime.js" });
const api = sandbox.BKCatRuntime;
assert.equal(typeof api.AgentRuntime, "function");
assert.equal(typeof api.piEngineFactory, "function");
assert.equal(typeof api.nativeSqlitePort, "function");

async function originalTransportContract(context = sandbox) {
  const { build } = await import(pathToFileURL(require.resolve("esbuild")).href);
  const output = await build({
    absWorkingDir: referenceRoot,
    stdin: { contents: [
      'export { McpClient } from "@bk-cat/pi-reference/mcp/client";',
      `export { NativeMcpTransport } from ${JSON.stringify(resolve(packageRoot, "src/native-adapters.ts"))};`,
      `export { ExecutionGate } from ${JSON.stringify(resolve(packageRoot, "src/gate.ts"))};`,
    ].join("\n"), loader: "ts", resolveDir: packageRoot },
    tsconfigRaw: { compilerOptions: { paths: referencePaths() } },
    nodePaths: [resolve(referenceRoot, "node_modules")],
    bundle: true, platform: "browser", target: "es2022", format: "iife", globalName: "PiTransportContract", write: false,
  });
  vm.runInContext(output.outputFiles[0].text, context, { filename: "pinned-client-contract.js" });
  return context.PiTransportContract;
}

test("original pinned McpClient preserves per-call argument identity for the native durable invocation side channel", async () => {
  const { McpClient, NativeMcpTransport, ExecutionGate } = await originalTransportContract();
  const lease = { storageId: "contract", generation: "1", handle: "contract-owner" };
  const gate = new ExecutionGate(); gate.acquiring(); gate.owned(lease);
  gate.activating(lease, { generation: "1", snapshotId: "reviewed", allowInitializeEmptyStore: false }); gate.ready(lease);
  const received = [];
  const transport = new NativeMcpTransport({ mcp: {
    async authorize(_lease, _message, _signal, invocation) { return { approved: true, ticket: `native-ticket-${invocation.callId}` }; },
    async send(_lease, message, _signal, invocation, approvalTicket) {
      if (message.method === "initialize") return { jsonrpc: "2.0", id: message.id, result: { protocolVersion: "2025-11-25", capabilities: { tools: {} }, serverInfo: { name: "explicit-fixture", version: "1" } } };
      if (message.method === "notifications/initialized") return undefined;
      assert.equal(message.method, "tools/call");
      assert.equal(message.params.name, "edit_apply_guarded");
      assert.equal(approvalTicket, `native-ticket-${invocation.callId}`);
      received.push({ arguments: message.params.arguments, invocation });
      return { jsonrpc: "2.0", id: message.id, result: { content: [{ type: "text", text: "Explicit protocol fixture receipt" }], isError: false } };
    },
  } }, lease, gate);
  const client = new McpClient({ name: "original-client-contract", version: "1", protocolVersion: "2025-11-25" });
  try {
    await client.connect(transport);
    const calls = [11, 12].map((taskId) => {
      const args = { expectedSession: 7, expectedRevision: 0, command: { op: "batch", commands: [] } };
      return transport.withInvocation(args, { conversationId: 9, taskId, callId: `stable-${taskId}` }, async () => {
        await transport.authorizeTool(args, new AbortController().signal);
        return client.callTool("edit_apply_guarded", args);
      });
    });
    const results = await Promise.all(calls);
    assert.equal(results.every((result) => result.isError === false), true);
    assert.deepEqual(received.map((entry) => ({ ...entry.invocation })), [
      { conversationId: 9, taskId: 11, callId: "stable-11" }, { conversationId: 9, taskId: 12, callId: "stable-12" },
    ]);
    assert.equal(received.some((entry) => "invocation" in entry.arguments || "operationId" in entry.arguments), false);
    await assert.rejects(client.callTool("edit_apply_guarded", received[0].arguments), /original Pi invocation/);
    assert.equal(received.length, 2);
  } finally { await client.close(); }
});

test("original MCP timeout starts after human approval and remains exactly thirty seconds for execution", async () => {
  let now = 0; let nextTimer = 0;
  const timers = new Map(); const scheduled = [];
  const context = vm.createContext({
    AbortController, AbortSignal, DOMException,
    setTimeout(callback, delay) { const id = ++nextTimer; scheduled.push(delay); timers.set(id, { callback, at: now + delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
  });
  const tick = (elapsed) => {
    now += elapsed;
    for (const [id, timer] of [...timers]) if (timer.at <= now) { timers.delete(id); timer.callback(); }
  };
  const { McpClient, NativeMcpTransport, ExecutionGate } = await originalTransportContract(context);
  const lease = { storageId: "clock-contract", generation: "1", handle: "clock-owner" };
  const gate = new ExecutionGate(); gate.acquiring(); gate.owned(lease);
  gate.activating(lease, { generation: "1", snapshotId: "reviewed", allowInitializeEmptyStore: false }); gate.ready(lease);
  let decide; let releaseTool; let toolDispatched;
  const approval = new Promise((resolve) => { decide = resolve; });
  const response = new Promise((resolve) => { releaseTool = resolve; });
  const dispatched = new Promise((resolve) => { toolDispatched = resolve; });
  let writes = 0;
  const transport = new NativeMcpTransport({ mcp: {
    async authorize() { return approval; },
    async send(_lease, message, _signal, invocation, ticket) {
      if (message.method === "initialize") return { jsonrpc: "2.0", id: message.id, result: { protocolVersion: "2025-11-25", capabilities: { tools: {} }, serverInfo: { name: "clock-fixture", version: "1" } } };
      if (message.method.startsWith("notifications/")) return undefined;
      writes++;
      assert.equal(ticket, "clock-native-ticket");
      assert.equal(invocation.callId, "clock-call");
      toolDispatched();
      await response;
      return { jsonrpc: "2.0", id: message.id, result: { content: [{ type: "text", text: "fixture receipt" }] } };
    },
  } }, lease, gate);
  const client = new McpClient({ name: "clock-client", version: "1", protocolVersion: "2025-11-25" });
  try {
    await client.connect(transport);
    scheduled.length = 0;
    const args = {};
    const pending = transport.withInvocation(args, { conversationId: 9, taskId: 11, callId: "clock-call" }, async () => {
      await transport.authorizeTool(args, new AbortController().signal);
      return client.callTool("edit_apply_guarded", args);
    });
    tick(60_001);
    assert.equal(writes, 0, "human waiting must not dispatch an MCP edit");
    assert.equal(timers.size, 0, "human waiting must not start a protocol timeout");
    assert.deepEqual(scheduled, []);
    decide({ approved: true, ticket: "clock-native-ticket" });
    await dispatched;
    assert.deepEqual(scheduled, [30_000], "execution retains the pinned client's unchanged timeout");
    let settled = false; void pending.catch(() => { settled = true; });
    tick(29_999); await Promise.resolve(); assert.equal(settled, false);
    tick(1);
    await assert.rejects(pending, /timed out|timeout/i);
    assert.equal(writes, 1, "no automatic retry after an execution timeout");
  } finally { releaseTool(); await client.close(); }
});

test("built browser bootstrap answers native commands and opens no writable store before native confirmation", async () => {
  const listeners = new Set();
  const sent = []; const nativeMethods = []; const waiting = new Map();
  let pagehide;
  const receive = (message) => { for (const listener of [...listeners]) listener({ data: JSON.stringify(message) }); };
  sandbox.addEventListener = (type, listener) => { if (type === "pagehide") pagehide = listener; };
  sandbox.BKCatNative = {
    addEventListener(_type, listener) { listeners.add(listener); },
    removeEventListener(_type, listener) { listeners.delete(listener); },
    postMessage(serialized) {
      const message = JSON.parse(serialized); sent.push(message);
      if (message.payload) {
        nativeMethods.push(message.payload.method);
        let result;
        if (message.payload.method === "ownership.acquire") result = { storageId: "bootstrap-isolated", generation: "4", handle: "bootstrap-owner" };
        else if (message.payload.method === "recovery.read") result = { snapshotId: "snapshot", projectSession: "1", projectRevision: "0", sessionRevision: "0", schemaVersion: "empty", hasInterruptedWork: false };
        else if (["ownership.revoke", "ownership.drainAndRelease"].includes(message.payload.method)) result = null;
        else throw new Error(`Unexpected native call before approval: ${message.payload.method}`);
        queueMicrotask(() => receive({ channel: "bk-cat-runtime-v1", id: message.id, result }));
      } else if (message.id && waiting.has(message.id)) {
        const { resolve, reject } = waiting.get(message.id); waiting.delete(message.id);
        if (message.error) reject(new Error(message.error.message)); else resolve(message.result);
      }
    },
  };
  const command = (id, payload) => new Promise((resolve, reject) => {
    waiting.set(id, { resolve, reject }); receive({ channel: "bk-cat-runtime-v1", id, payload });
  });
  vm.runInContext(readFileSync(resolve(packageRoot, "dist/bootstrap.js"), "utf8"), sandbox, { filename: "bootstrap.js" });
  assert.ok(sent.some((message) => message.event === "runtime.loaded" && message.modelTransport === "native-scoped-stream"));
  try {
    const started = await command("native-start", { method: "runtime.start", storageId: "bootstrap-isolated" });
    assert.equal(started.schemaVersion, "empty");
    assert.deepEqual(nativeMethods, ["ownership.acquire", "recovery.read"]);
    await assert.rejects(command("native-start", { method: "runtime.start", storageId: "bootstrap-isolated" }), /repeated/);
    await assert.rejects(command("native-disallowed-resume", { method: "runtime.dispatch", action: "resume" }), /reconciliation/);
    await command("native-stop", { method: "runtime.stop" });
    assert.deepEqual(nativeMethods, ["ownership.acquire", "recovery.read", "ownership.revoke", "ownership.drainAndRelease"]);
    await assert.rejects(command("native-restart", { method: "runtime.start", storageId: "bootstrap-isolated" }), /one host/);
    assert.equal(forbiddenFetch, 0);
  } finally { pagehide?.(); delete sandbox.BKCatNative; }
});
function sqliteFixture(file, generation, options = {}) {
  let connection;
  let transaction;
  let revoked = false;
  const databaseId = `db-${generation}`;
  const calls = [];
  const committedTasks = [];
  const wire = {
    async send(frame, signal) {
      calls.push(frame);
      assert.equal(frame.generation, String(generation));
      assert.equal(frame.ownerHandle, `owner-${generation}`);
      if (!["sqlite.close", "sqlite.rollback"].includes(frame.method)) {
        assert.equal(revoked, false, "revoked SQLite write/read rejected");
        signal?.throwIfAborted();
      }
      if (frame.method === "sqlite.open") {
        assert.equal(frame.expectedSchemaVersion, 2);
        assert.equal("path" in frame, false);
        if (!existsSync(file)) assert.equal(frame.allowInitializeEmptyStore, true);
        connection = new DatabaseSync(file);
        return { databaseId };
      }
      assert.equal(frame.databaseId, databaseId);
      if (frame.method === "sqlite.close") { connection.close(); connection = undefined; return null; }
      if (frame.method === "sqlite.begin") {
        assert.equal(transaction, undefined);
        connection.exec("BEGIN IMMEDIATE");
        transaction = `transaction-${calls.length}`;
        return { transactionId: transaction };
      }
      assert.equal(frame.transactionId, transaction);
      if (frame.method === "sqlite.commit" || frame.method === "sqlite.rollback") {
        connection.exec(frame.method === "sqlite.commit" ? "COMMIT" : "ROLLBACK");
        transaction = undefined;
        if (options.traceTasks && frame.method === "sqlite.commit"
          && connection.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name = 'tasks'").get()) {
          committedTasks.push(connection.prepare("SELECT record FROM tasks ORDER BY id").all().map((row) => JSON.parse(row.record)));
        }
        return null;
      }
      if (frame.method === "sqlite.exec") { connection.exec(frame.sql); return null; }
      const statement = connection.prepare(frame.sql);
      const params = (frame.params ?? []).map((value) => value?.type === "integer" ? BigInt(value.value)
        : value?.type === "blob" ? Uint8Array.from(Buffer.from(value.base64, "base64")) : value);
      if (frame.method === "sqlite.run" || frame.method === "sqlite.runCounted") {
        const result = statement.run(...params);
        return frame.method === "sqlite.runCounted" ? Number(result.changes) : null;
      }
      if (frame.method === "sqlite.get") return statement.get(...params) ?? null;
      if (frame.method === "sqlite.all") return statement.all(...params);
      throw new Error(`Unexpected SQLite method ${frame.method}`);
    },
  };
  return { calls, wire, committedTasks, revoke: () => { revoked = true; }, isClosed: () => !connection };
}

function fixture(file, generation, configured, transport, options = {}) {
  const sqlite = sqliteFixture(file, generation, options);
  const mcpCalls = [];
  const approvalCalls = [];
  let granted = false;
  let networkCalls = 0;
  let credentialReads = 0;
  let snapshotReads = 0;
  let verificationCalls = 0;
  const taskBindings = [];
  const lease = { storageId: "isolated-pi", generation: String(generation), handle: `owner-${generation}` };
  const snapshot = () => ({
    snapshotId: `reconciled-${generation}`, projectSession: "1", projectRevision: "0", sessionRevision: "0",
    schemaVersion: existsSync(file) ? 2 : "empty", hasInterruptedWork: options.interrupted === true,
  });
  const ports = {
    ownership: {
      async acquire(id) { assert.equal(id, lease.storageId); return lease; },
      async confirm(_lease, read, approval) {
        assert.equal(approval.snapshotId, read.snapshotId); granted = true;
        if (options.discardConversationIds !== undefined) assert.equal(approval.decision, "discard");
        return { generation: lease.generation, snapshotId: read.snapshotId, allowInitializeEmptyStore: read.schemaVersion === "empty",
          ...(options.discardConversationIds === undefined ? {} : { discardConversationIds: options.discardConversationIds }) };
      },
      async revoke() { sqlite.revoke(); },
      async drainAndRelease() { assert.equal(sqlite.isClosed(), true, "SQLite closes before ownership release"); },
      onRevoked() { return () => {}; },
    },
    recovery: {
      async read() { snapshotReads++; return snapshot(); },
      async discardVerified() {
        verificationCalls++;
        assert.ok(options.discardConversationIds, "only explicit native discard can request completion verification");
        assert.equal(runtime.phase, "activating");
        const reader = new DatabaseSync(file, { readOnly: true });
        try {
          assert.equal(reader.prepare("SELECT count(*) AS n FROM tasks WHERE status != 'terminal'").get().n, 0);
          assert.equal(reader.prepare("SELECT count(*) AS n FROM submissions WHERE status NOT IN ('done', 'unanswered')").get().n, 0);
        } finally { reader.close(); }
        await options.verifyDiscard?.();
      },
    },
    sqlite: { async open(...args) { assert.equal(granted, true); return api.nativeSqlitePort(sqlite.wire).open(...args); } },
    mcp: {
      async authorize(_lease, message, signal, invocation) {
        approvalCalls.push({ message, invocation });
        assert.ok(taskBindings.some((binding) => binding.conversationId === invocation.conversationId));
        if (!options.authorize) throw new Error("This fixture does not authorize edits");
        return options.authorize(message, signal, invocation);
      },
      async send(_lease, message, _signal, invocation, approvalTicket) {
        mcpCalls.push(message);
        if (message.method === "initialize") {
          assert.equal(message.params.protocolVersion, "2025-11-25");
          return nativeJsonReply({ jsonrpc: "2.0", id: message.id, result: { protocolVersion: "2025-11-25", serverInfo: { name: "explicit-fixture", version: "1" }, capabilities: { tools: {} } } });
        }
        if (["notifications/initialized", "notifications/cancelled"].includes(message.method)) return undefined;
        // The real NativeWire receives JSON in the web realm. Preserve that
        // boundary even in this direct-port fixture; host-realm arrays are
        // rejected by the original durable strict-JSON checks.
        if (message.method === "tools/list") return nativeJsonReply({ jsonrpc: "2.0", id: message.id, result: { tools: [
          "concat_version", "project_get", "timeline_read", "effects_list", "template_list", "edit_apply_guarded",
        ].map((name) => ({ name, description: "Protocol fixture only", inputSchema: { type: "object", properties: {}, ...(name === "edit_apply_guarded" ? { additionalProperties: true } : {}) } })) } });
        if (transport && message.method === "tools/call") {
          assert.ok(taskBindings.some((binding) => binding.conversationId === invocation.conversationId));
          if (options.tool) return options.tool(message, _signal, invocation, approvalTicket);
          assert.equal(message.params.name, "project_get", "only the fixture's real read path is implemented");
          return nativeJsonReply({ jsonrpc: "2.0", id: message.id, result: { content: [{ type: "text", text: "Explicit read-only project fixture" }] } });
        }
        throw new Error(`Unexpected model/native tool call ${message.method}`);
      },
    },
    network: { async fetch(...args) {
      networkCalls++;
      if (transport) {
        assert.ok(taskBindings.length, "native task binds before original model scheduling");
        return transport(...args);
      }
      throw new Error("No model service is authorized");
    } },
    tasks: { async bind(_lease, taskId, conversationId) { taskBindings.push({ taskId, conversationId }); } },
  };
  const configuration = { providers: [], credentials: {
    async read() { credentialReads++; throw new Error("No credentials are configured"); },
    async list() { throw new Error("No credential enumeration"); },
    async modify() { throw new Error("No credential modification"); },
    async delete() { throw new Error("No credential deletion"); },
  } };
  const runtime = new api.AgentRuntime(lease.storageId, ports, api.piEngineFactory(configured ?? configuration));
  return { runtime, sqlite, mcpCalls, approvalCalls, taskBindings, snapshotReads: () => snapshotReads, networkCalls: () => networkCalls,
    credentialReads: () => credentialReads, verificationCalls: () => verificationCalls };
}

test("built pinned original Harness persists real conversation records in SQLite and reopens only after explicit reconciliation", { timeout: 15000 }, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), "bkcat-original-pi-"));
  const file = resolve(directory, "pi-session.sqlite3");
  const first = fixture(file, 1);
  let second;
  try {
    const snapshot = await first.runtime.start();
    assert.equal(first.runtime.phase, "recovery");
    assert.equal(existsSync(file), false, "read-only startup creates no SQLite file");
    assert.equal(first.sqlite.calls.length, 0);
    assert.equal(first.mcpCalls.length, 0);
    await first.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "continue", approvalId: "native-test-approval" });
    assert.equal(first.runtime.phase, "ready");
    assert.equal(existsSync(file), true);
    assert.deepEqual(first.mcpCalls.map((message) => message.method), ["initialize", "notifications/initialized", "tools/list"]);
    const created = await first.runtime.dispatch("createConversation", {});
    assert.equal(Number.isSafeInteger(created.id), true);
    const events = [];
    const observation = await first.runtime.observeConversation(created.id, (event) => events.push(event));
    assert.equal(observation.value.conversation.id, created.id);
    const submitted = await first.runtime.dispatch("submit", { conversationId: created.id, submission: { type: "input", requestId: "native-id-1", content: "Persist this real user message" } });
    const status = await first.runtime.dispatch("waitSubmission", { submissionId: submitted.id });
    assert.equal(status.status, "unanswered");
    assert.equal(status.reason, "no_model");
    observation.detach();
    assert.ok(events.some((event) => event.kind === "value"), "original view publishes real transcript updates");
    assert.equal(first.networkCalls(), 0);
    assert.equal(first.credentialReads(), 0);
    await first.runtime.stop();
    const stable = readFileSync(file);
    const reader = new DatabaseSync(file, { readOnly: true });
    try {
      assert.equal(reader.prepare("SELECT version FROM durable_schema WHERE singleton=1").get().version, 2);
      const stored = reader.prepare("SELECT record FROM entries ORDER BY id").all().map((row) => JSON.parse(row.record));
      assert.ok(stored.some((entry) => entry.model?.some((message) => message.role === "user" && message.content === "Persist this real user message")));
      assert.equal(stored.some((entry) => entry.model?.some((message) => message.role === "assistant")), false, "no synthetic assistant response");
    } finally { reader.close(); }
    second = fixture(file, 2);
    const secondSnapshot = await second.runtime.start();
    assert.equal(second.runtime.phase, "recovery");
    assert.equal(second.sqlite.calls.length, 0);
    assert.equal(second.mcpCalls.length, 0);
    assert.deepEqual(readFileSync(file), stable, "recovery startup does not mutate the persisted store");
    await assert.rejects(second.runtime.dispatch("resume"));
    await second.runtime.confirmRecovery({ snapshotId: secondSnapshot.snapshotId, decision: "continue", approvalId: "new-native-test-approval" });
    const reopened = await second.runtime.observeConversation(created.id, () => {});
    assert.ok(reopened.value.entries.some((entry) => entry.model?.some((message) => message.role === "user" && message.content === "Persist this real user message")));
    reopened.detach();
    assert.equal(second.networkCalls(), 0);
    assert.equal(second.credentialReads(), 0);
    assert.equal(forbiddenFetch, 0);
    await second.runtime.stop();
  } finally {
    await first.runtime.stop().catch(() => {});
    await second?.runtime.stop().catch(() => {});
    rmSync(directory, { recursive: true, force: true });
  }
});

const nativeConfiguration = (protocol = "openai-completions") => ({
  id: "native-channel", version: "7", configurationVersion: "9", protocol,
  endpoint: "https://model.invalid/custom", model: "explicit-model", requestTimeoutMs: 10000, maxOutputTokens: 128,
});

function completionResponse(parts) {
  const chunks = parts.map((part) => `data: ${JSON.stringify({
    id: "fixture-completion", object: "chat.completion.chunk", created: 1, model: "explicit-model",
    choices: [{ index: 0, delta: part.delta ?? {}, finish_reason: part.finish ?? null }],
  })}\n\n`).join("") + "data: [DONE]\n\n";
  return new Response(chunks, { headers: { "content-type": "text/event-stream" } });
}

// The pinned Pi scheduler must run beyond the old cap, even when the saved
// conversation still contains the previous four-turn setting.
test("native model task clears legacy four-turn cap and completes eight sequential requests", { timeout: 30000 }, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), "bkcat-unlimited-pi-"));
  const file = resolve(directory, "session.sqlite3");
  let requests = 0;
  const instance = fixture(file, 1, api.nativeModelConfiguration(nativeConfiguration()), async () => {
    requests++;
    return requests <= 7
      ? completionResponse([
        { delta: { role: "assistant", tool_calls: [{ index: 0, id: `read-${requests}`, type: "function", function: { name: "project_get", arguments: "{}" } }] } },
        { finish: "tool_calls" },
      ])
      : completionResponse([{ delta: { role: "assistant", content: "八轮调用完成" } }, { finish: "stop" }]);
  });
  try {
    const snapshot = await instance.runtime.start();
    await instance.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "continue", approvalId: "explicit-unlimited-test" });
    const legacy = await instance.runtime.dispatch("createConversation", {
      agent: { model: { provider: "native-channel", modelId: "explicit-model" }, maxTurns: 4 },
    });
    const taskId = "unlimited-native-task";
    await instance.runtime.dispatch("nativeSubmit", {
      taskId, networkPermit: "bounded-fixture-permit", conversationId: legacy.id,
      input: "连续读取工程，直到完成任务", mode: "chat",
    });
    const result = await instance.runtime.dispatch("nativeWait", { taskId });
    assert.equal(result.status, "done", result.status === "done" ? undefined : durableFailure(file));
    assert.equal(result.text, "八轮调用完成");
    assert.equal(instance.networkCalls(), 8, "seven tool rounds plus a final answer must exceed the old limit");
    assert.equal(instance.mcpCalls.filter((message) => message.method === "tools/call").length, 7);
    assert.equal(forbiddenFetch, 0);
  } finally {
    await instance.runtime.stop().catch(() => {});
    rmSync(directory, { recursive: true, force: true });
  }
});

// Only deterministic fixtures use this reader. Keep the original failure
// detail in a failing assertion before its temporary database is cleaned up;
// no service credentials or live model response enter this test environment.
function durableFailure(file) {
  const reader = new DatabaseSync(file, { readOnly: true });
  try {
    const submissions = reader.prepare("SELECT record FROM submissions ORDER BY id").all()
      .map((row) => JSON.parse(row.record))
      .map(({ id, status, reason, detail }) => ({ id, status, reason, detail }));
    const tools = reader.prepare("SELECT record FROM entries ORDER BY id").all()
      .map((row) => JSON.parse(row.record))
      .flatMap((entry) => entry.model ?? []).filter((message) => message.role === "toolResult");
    return JSON.stringify({ submissions, tools });
  } finally { reader.close(); }
}

function durableRecords(file) {
  const reader = new DatabaseSync(file, { readOnly: true });
  try {
    return Object.fromEntries(["tasks", "submissions", "entries", "conversations", "documents"].map((table) => [
      table, reader.prepare(`SELECT * FROM ${table} ORDER BY id`).all(),
    ]));
  } finally { reader.close(); }
}

/** This seeds real interrupted records by stopping the production original
 * tool path, not by synthesizing a task graph or setting a fake success. */
async function interruptedToolStore(file) {
  let entered;
  const toolEntered = new Promise((resolve) => { entered = resolve; });
  const first = fixture(file, 1, api.nativeModelConfiguration(nativeConfiguration()), async () => completionResponse([
    { delta: { role: "assistant", tool_calls: [{ index: 0, id: "interrupted-project-read", type: "function", function: { name: "project_get", arguments: "{}" } }] } },
    { finish: "tool_calls" },
  ]), {
    tool(_message, signal, invocation) {
      return new Promise((_resolve, reject) => {
        signal.throwIfAborted();
        signal.addEventListener("abort", () => reject(signal.reason), { once: true });
        entered(invocation);
      });
    },
  });
  try {
    const snapshot = await first.runtime.start();
    await first.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "continue", approvalId: "native-seed" });
    const admitted = await first.runtime.dispatch("nativeSubmit", {
      taskId: "interrupted-read", networkPermit: "seed-only-permit", input: "保留这条被停止的真实输入", mode: "chat",
    });
    const invocation = await toolEntered;
    assert.equal(invocation.conversationId, admitted.conversationId);
    const running = durableRecords(file).tasks.map((row) => JSON.parse(row.record));
    assert.ok(running.some((record) => record.kind === "pi.tool" && record.state.status === "running"));
    await first.runtime.stop();
    assert.equal(first.sqlite.isClosed(), true);
    assert.equal(first.networkCalls(), 1);
    assert.equal(first.mcpCalls.filter((message) => message.method === "tools/call").length, 1);
    const records = durableRecords(file);
    const live = records.tasks.map((row) => JSON.parse(row.record)).filter((record) => record.state.status !== "terminal");
    assert.equal(live.length, 2, "the original generation and its actual tool both remain interrupted");
    assert.ok(live.some((record) => record.kind === "pi.generation"));
    assert.ok(live.some((record) => record.kind === "pi.tool" && record.state.status === "running"),
      "native revoke blocks close-time writes, so the real interrupted running record is preserved");
    assert.ok(records.submissions.some((row) => row.id === admitted.submissionId && row.status === "placed"));
    return { admitted, records, live };
  } finally { await first.runtime.stop().catch(() => {}); }
}

test("explicit current-project discard marks every original task before cancellation-only draining, preserves history and permits a new explicit request", { timeout: 20000 }, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), "bkcat-original-discard-"));
  const file = resolve(directory, "session.sqlite3");
  let reopened;
  try {
    const seeded = await interruptedToolStore(file);
    reopened = fixture(file, 2, api.nativeModelConfiguration(nativeConfiguration()), async (_lease, permit) => {
      assert.equal(permit, "new-explicit-permit");
      return completionResponse([{ delta: { role: "assistant", content: "明确新任务的答案" } }, { finish: "stop" }]);
    }, { interrupted: true, discardConversationIds: [seeded.admitted.conversationId], traceTasks: true });
    const snapshot = await reopened.runtime.start();
    await assert.rejects(reopened.runtime.dispatch("nativeSubmit", {}), /reconciliation/);
    assert.deepEqual(durableRecords(file), seeded.records, "startup alone must not reconcile or change a running record");
    await reopened.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "discard", approvalId: "explicit-native-discard" });
    assert.equal(reopened.runtime.phase, "ready");
    assert.equal(reopened.verificationCalls(), 1);
    assert.equal(reopened.networkCalls(), 0, "original abort handlers must not request a model");
    assert.equal(reopened.mcpCalls.filter((message) => message.method === "tools/call").length, 0, "old tools must not execute");
    assert.equal(reopened.taskBindings.length, 0, "discard has no native model/task authorization");
    const after = durableRecords(file);
    const tasks = after.tasks.map((row) => JSON.parse(row.record));
    for (const original of seeded.live) {
      const ended = tasks.find((record) => record.id === original.id);
      assert.equal(ended.state.status, "terminal");
      assert.equal(ended.state.outcome.status, "aborted");
    }
    const commits = reopened.sqlite.committedTasks;
    const ids = new Set(seeded.live.map((record) => record.id));
    const firstTerminal = commits.findIndex((records) => records.some((record) => ids.has(record.id) && record.state.status === "terminal"));
    assert.ok(firstTerminal > 0, "the test observed real terminal SQLite commits");
    assert.ok(commits.slice(0, firstTerminal).some((records) => seeded.live.every(({ id }) => {
      const record = records.find((entry) => entry.id === id);
      return record?.abortRequested === true && record.state.status !== "terminal";
    })), "a committed all-task cancellation barrier must precede the first terminal task");
    for (const entry of seeded.records.entries) assert.deepEqual(after.entries.find((row) => row.id === entry.id), entry);
    const oldInput = after.submissions.map((row) => JSON.parse(row.record)).find((record) => record.id === seeded.admitted.submissionId);
    assert.equal(oldInput.status, "unanswered"); assert.equal(oldInput.reason, "aborted");
    const observed = await reopened.runtime.observeConversation(seeded.admitted.conversationId, () => {});
    assert.ok(observed.value.entries.some((entry) => entry.model?.some((message) => message.role === "user" && message.content === "保留这条被停止的真实输入")));
    observed.detach();
    const next = await reopened.runtime.dispatch("nativeSubmit", {
      taskId: "new-explicit-task", networkPermit: "new-explicit-permit", conversationId: seeded.admitted.conversationId,
      input: "这是明确的新请求", mode: "chat",
    });
    const result = await reopened.runtime.dispatch("nativeWait", { taskId: "new-explicit-task" });
    assert.equal(result.status, "done", result.status === "done" ? undefined : durableFailure(file));
    assert.equal(result.text, "明确新任务的答案");
    assert.equal(result.conversationId, seeded.admitted.conversationId); assert.equal(result.submissionId, next.submissionId);
    assert.equal(reopened.networkCalls(), 1, "only the newly approved task contacts the provider fixture");
    assert.equal(reopened.taskBindings.length, 1); assert.equal(forbiddenFetch, 0);
    await reopened.runtime.stop();
  } finally { await reopened?.runtime.stop().catch(() => {}); rmSync(directory, { recursive: true, force: true }); }
});

test("original-store discard rejects other projects, unknown records and deferred polling before changing any running task or submission", { timeout: 30000 }, async (t) => {
  const directory = mkdtempSync(resolve(tmpdir(), "bkcat-original-discard-refused-"));
  try {
    const seedFile = resolve(directory, "seed.sqlite3");
    const seeded = await interruptedToolStore(seedFile);
    const cases = ["other-project", "unknown-version", "unknown-kind", "poll", "retry-poll", "unknown-task-state", "unknown-submission-state", "unknown-submission-kind", "other-project-submission", "late-unknown-state"];
    for (const label of cases) await t.test(label, async () => {
      const file = resolve(directory, `${label}.sqlite3`); copyFileSync(seedFile, file);
      if (label !== "other-project") {
        // Deliberately corrupt only this deterministic fixture. Production
        // never writes Pi task state with raw SQL or bypasses its constraints.
        const writer = new DatabaseSync(file);
        try {
          const generation = writer.prepare("SELECT * FROM tasks ORDER BY id").all()
            .map((row) => JSON.parse(row.record)).find((record) => record.kind === "pi.generation");
          if (["unknown-submission-state", "unknown-submission-kind", "other-project-submission"].includes(label)) {
            const submission = JSON.parse(writer.prepare("SELECT record FROM submissions WHERE id = ?").get(seeded.admitted.submissionId).record);
            if (label === "unknown-submission-state") { writer.exec("PRAGMA ignore_check_constraints = ON"); submission.status = "future-submission-state"; }
            if (label === "unknown-submission-kind") submission.type = "future-submission-kind";
            if (label === "other-project-submission") submission.conversationId += 100;
            writer.prepare("UPDATE submissions SET status = ?, conversation_id = ?, record = ? WHERE id = ?")
              .run(submission.status, submission.conversationId, JSON.stringify(submission), submission.id);
          } else if (label === "late-unknown-state") {
            writer.exec("PRAGMA ignore_check_constraints = ON");
            const insert = writer.prepare("INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record) VALUES (?, ?, ?, ?, ?, ?, ?)");
            for (let index = 0; index < 300; index++) {
              const terminal = { ...generation, id: 10000 + index, conversationId: seeded.admitted.conversationId + 100,
                state: { status: "terminal", outcome: { status: "aborted" } } };
              insert.run(terminal.id, terminal.conversationId, terminal.kind, "terminal", 0, 0, JSON.stringify(terminal));
            }
            const future = { ...generation, id: 20000, state: { ...generation.state, status: "future-task-state" } };
            insert.run(future.id, future.conversationId, future.kind, "future-task-state", 0, 0, JSON.stringify(future));
          } else {
            if (label === "unknown-version") generation.version = 999;
            if (label === "unknown-kind") generation.kind = "future-generation-task";
            if (label === "poll") generation.state.checkpoint = { phase: "poll", provider: "native-channel", responseId: "must-not-contact" };
            if (label === "retry-poll") generation.state.checkpoint = { phase: "retry", poll: { provider: "native-channel", responseId: "must-not-contact" } };
            if (label === "unknown-task-state") { writer.exec("PRAGMA ignore_check_constraints = ON"); generation.state.status = "future-task-state"; }
            writer.prepare("UPDATE tasks SET kind = ?, status = ?, record = ? WHERE id = ?")
              .run(generation.kind, generation.state.status, JSON.stringify(generation), generation.id);
          }
        } finally { writer.close(); }
      }
      const before = durableRecords(file);
      assert.ok(before.tasks.some((row) => JSON.parse(row.record).state.status === "running"), "every rejection fixture contains an actual interrupted running tool");
      const currentConversation = seeded.admitted.conversationId + (label === "other-project" ? 1 : 0);
      const f = fixture(file, 2, api.nativeModelConfiguration(nativeConfiguration()), async () => {
        throw new Error("rejected discard must not request a model");
      }, { interrupted: true, discardConversationIds: [currentConversation], traceTasks: true });
      try {
        const snapshot = await f.runtime.start();
        await assert.rejects(f.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "discard", approvalId: `native-refuse-${label}` }),
          /其他工程|未经审核|延期网络轮询|未知历史任务状态/);
        assert.equal(f.runtime.phase, "closed");
        assert.deepEqual(durableRecords(file), before, "all original task/submission/history records must remain byte-for-byte equal after rejection");
        assert.equal(f.networkCalls(), 0); assert.equal(f.credentialReads(), 0); assert.equal(f.taskBindings.length, 0);
        assert.equal(f.mcpCalls.filter((message) => message.method === "tools/call").length, 0);
        assert.equal(f.verificationCalls(), 0, "a rejected original preflight cannot claim native cleanup completion");
        if (label === "late-unknown-state") assert.ok(f.sqlite.calls.filter((frame) => frame.method === "sqlite.all" && frame.sql.includes("FROM tasks WHERE id >")).length >= 2,
          "an invalid record beyond the first page must be read and refused too");
        assert.equal(forbiddenFetch, 0);
      } finally { await f.runtime.stop().catch(() => {}); }
    });
  } finally { rmSync(directory, { recursive: true, force: true }); }
});

test("configured original provider submits, persists a real generated answer and reopens it without another request", { timeout: 15000 }, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), "bkcat-native-model-"));
  const file = resolve(directory, "session.sqlite3");
  const configured = api.nativeModelConfiguration(nativeConfiguration());
  const requests = [];
  const first = fixture(file, 1, configured, async (_lease, permit, input, init) => {
    assert.equal(permit, "only-this-task");
    const request = new Request(input, init);
    const body = JSON.parse(await request.text()); requests.push(body);
    assert.equal(request.url, "https://model.invalid/custom/chat/completions");
    assert.equal(body.model, "explicit-model"); assert.equal(body.stream, true);
    assert.equal(body.max_tokens ?? body.max_completion_tokens, 128);
    assert.equal(body.tools.some((tool) => tool.function?.name === "edit_apply_guarded"), false);
    return completionResponse([{ delta: { role: "assistant", content: "独立候选正文" } }, { finish: "stop" }]);
  });
  let second;
  try {
    const snapshot = await first.runtime.start();
    await first.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "continue", approvalId: "native-recovery" });
    assert.equal(first.networkCalls(), 0, "opening a configured runtime does not authorize or contact its model");
    const admitted = await first.runtime.dispatch("nativeSubmit", {
      taskId: "native-task-1", networkPermit: "only-this-task", input: "生成一个短视频文案", mode: "script",
    });
    const final = await first.runtime.dispatch("nativeWait", { taskId: "native-task-1" });
    assert.equal(final.status, "done", final.status === "done" ? undefined : durableFailure(file));
    assert.equal(final.text, "独立候选正文");
    assert.equal(final.submissionId, admitted.submissionId); assert.equal(final.conversationId, admitted.conversationId);
    assert.equal(first.taskBindings.length, 1); assert.equal(requests.length, 1);
    await first.runtime.stop();
    const stored = readFileSync(file);
    second = fixture(file, 2, configured, async () => { throw new Error("reopen must not contact model"); });
    const next = await second.runtime.start();
    assert.deepEqual(readFileSync(file), stored);
    await second.runtime.confirmRecovery({ snapshotId: next.snapshotId, decision: "continue", approvalId: "native-reopen" });
    const observed = await second.runtime.observeConversation(admitted.conversationId, () => {});
    assert.ok(observed.value.entries.some((entry) => entry.model?.some((message) => message.role === "assistant" && message.content?.some((part) => part.type === "text" && part.text === "独立候选正文"))));
    observed.detach();
    assert.equal(second.networkCalls(), 0); assert.equal(forbiddenFetch, 0);
    await second.runtime.stop();
  } finally {
    await first.runtime.stop().catch(() => {}); await second?.runtime.stop().catch(() => {});
    rmSync(directory, { recursive: true, force: true });
  }
});

test("original model tool round carries native-bound Pi invocation and stays read-only for script generation", { timeout: 15000 }, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), "bkcat-model-tools-"));
  let requests = 0;
  const f = fixture(resolve(directory, "session.sqlite3"), 1, api.nativeModelConfiguration(nativeConfiguration()), async (_lease, _permit, input, init) => {
    requests++;
    if (requests === 1) return completionResponse([
      { delta: { role: "assistant", tool_calls: [{ index: 0, id: "read-project", type: "function", function: { name: "project_get", arguments: "{}" } }] } },
      { finish: "tool_calls" },
    ]);
    const body = JSON.parse(await new Request(input, init).text());
    assert.ok(body.messages.some((message) => message.role === "tool"
      && message.content === "Explicit read-only project fixture"), "the second model request must receive the successful original tool result");
    return completionResponse([{ delta: { role: "assistant", content: "读取工程后的候选正文" } }, { finish: "stop" }]);
  });
  try {
    const snapshot = await f.runtime.start();
    await f.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "continue", approvalId: "native-recovery" });
    await f.runtime.dispatch("nativeSubmit", { taskId: "native-read-task", networkPermit: "read-permit", input: "读取工程并生成文案", mode: "script" });
    const completed = await f.runtime.dispatch("nativeWait", { taskId: "native-read-task" });
    assert.equal(completed.status, "done", completed.status === "done" ? undefined : durableFailure(resolve(directory, "session.sqlite3")));
    assert.equal(completed.text, "读取工程后的候选正文");
    assert.equal(requests, 2);
    const calls = f.mcpCalls.filter((message) => message.method === "tools/call");
    assert.equal(calls.length, 1, calls.length === 1 ? undefined : durableFailure(resolve(directory, "session.sqlite3")));
    assert.equal(forbiddenFetch, 0);
  } finally { await f.runtime.stop().catch(() => {}); rmSync(directory, { recursive: true, force: true }); }
});

test("original durable edit task resumes the same conversation after allow, refusal or approval expiry", { timeout: 20000 }, async () => {
  for (const outcome of ["allow", "deny", "expire"]) {
    const directory = mkdtempSync(resolve(tmpdir(), `bkcat-approval-${outcome}-`));
    let decide; let approvalStarted;
    const decision = new Promise((resolve) => { decide = resolve; });
    const waiting = new Promise((resolve) => { approvalStarted = resolve; });
    let requests = 0; let writes = 0;
    const receipt = outcome === "allow" ? "原生测试回执：文字已添加" : outcome === "deny" ? "用户拒绝了本次剪辑操作，工程未修改。" : "等待剪辑批准超时，工程未修改。";
    const args = { path: "isolated-project", expectedSession: 7, expectedRevision: 0,
      command: { op: "addTextClip", trackId: "T3", start: 1, duration: 2, style: { content: "审批测试文字" } } };
    const f = fixture(resolve(directory, "session.sqlite3"), 1, api.nativeModelConfiguration(nativeConfiguration()), async (_lease, _permit, input, init) => {
      requests++;
      if (requests === 1) return completionResponse([
        { delta: { role: "assistant", tool_calls: [{ index: 0, id: "original-edit-call", type: "function", function: { name: "edit_apply_guarded", arguments: JSON.stringify(args) } }] } },
        { finish: "tool_calls" },
      ]);
      const body = JSON.parse(await new Request(input, init).text());
      assert.ok(body.messages.some((message) => message.role === "tool" && message.content === receipt), "the original model continuation receives the actual approval outcome");
      return completionResponse([{ delta: { role: "assistant", content: `原会话处理完成：${outcome}` } }, { finish: "stop" }]);
    }, {
      async authorize(message, signal, invocation) {
        assert.equal(message.id, undefined);
        assert.equal(message.params.name, "edit_apply_guarded");
        assert.deepEqual(JSON.parse(JSON.stringify(message.params.arguments)), args);
        assert.equal(invocation.callId, "original-edit-call");
        assert.equal(signal.aborted, false);
        approvalStarted(); return decision;
      },
      async tool(message, _signal, invocation, ticket) {
        writes++;
        assert.equal(outcome, "allow", "a refused or expired approval never dispatches its edit");
        assert.equal(ticket, "native-approved-ticket");
        assert.equal(invocation.callId, "original-edit-call");
        return nativeJsonReply({ jsonrpc: "2.0", id: message.id, result: { content: [{ type: "text", text: receipt }], isError: false } });
      },
    });
    try {
      const snapshot = await f.runtime.start();
      await f.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "continue", approvalId: "native-recovery" });
      const submitted = await f.runtime.dispatch("nativeSubmit", { taskId: `approval-${outcome}`, networkPermit: "native-permit", input: "新增文字并等待用户批准", mode: "edit" });
      const completion = f.runtime.dispatch("nativeWait", { taskId: `approval-${outcome}` });
      await waiting;
      assert.equal(f.runtime.phase, "ready");
      assert.equal(f.mcpCalls.filter((message) => message.method === "tools/call").length, 0);
      assert.equal(requests, 1);
      decide(outcome === "allow" ? { approved: true, ticket: "native-approved-ticket" } : { approved: false, message: receipt });
      const completed = await completion;
      assert.equal(completed.status, "done", durableFailure(resolve(directory, "session.sqlite3")));
      assert.equal(completed.conversationId, submitted.conversationId);
      assert.equal(completed.submissionId, submitted.submissionId);
      assert.equal(completed.text, `原会话处理完成：${outcome}`);
      assert.equal(writes, outcome === "allow" ? 1 : 0);
      assert.equal(f.approvalCalls.length, 1);
      assert.equal(requests, 2);
      assert.equal(f.runtime.phase, "ready");
    } finally {
      decide({ approved: false, message: "test cleanup" });
      await f.runtime.stop().catch(() => {});
      rmSync(directory, { recursive: true, force: true });
    }
  }
});

test("all three original adapters use the explicit service base and output ceiling with no implicit model retry", { timeout: 20000 }, async () => {
  for (const [protocol, suffix, outputField, override] of [
    ["openai-completions", "/chat/completions", "max_completion_tokens"],
    ["openai-responses", "/responses", "max_output_tokens"],
    ["anthropic-messages", "/v1/messages?beta=true", "max_tokens"],
    ["openai-responses", "/responses", "max_output_tokens", { id: "openai", endpoint: "https://api.openai.com/v1" }],
    ["openai-completions", "/chat/completions", "max_completion_tokens", { endpoint: "https://model.invalid/custom///" }],
  ]) {
    const directory = mkdtempSync(resolve(tmpdir(), "bkcat-original-adapter-"));
    const captured = [];
    const configuration = { ...nativeConfiguration(protocol), ...override };
    const f = fixture(resolve(directory, "session.sqlite3"), 1, api.nativeModelConfiguration(configuration), async (_lease, _permit, input, init) => {
      const request = new Request(input, init);
      captured.push({ url: request.url, body: JSON.parse(await request.text()) });
      throw new Error("native model request refused by deterministic fixture");
    });
    try {
      const snapshot = await f.runtime.start();
      await f.runtime.confirmRecovery({ snapshotId: snapshot.snapshotId, decision: "continue", approvalId: "native-recovery" });
      await f.runtime.dispatch("nativeSubmit", { taskId: `task-${protocol}`, networkPermit: "one-permit", input: "测试渠道", mode: "chat" });
      const done = await f.runtime.dispatch("nativeWait", { taskId: `task-${protocol}` });
      assert.equal(done.status, "unanswered"); assert.equal(done.text, "");
      assert.equal(captured.length, 1, captured.length === 1 ? protocol : `${protocol}: ${durableFailure(resolve(directory, "session.sqlite3"))}`);
      assert.equal(captured[0].url, `${configuration.endpoint.replace(/\/+$/, "")}${suffix}`);
      assert.equal(captured[0].body.model, "explicit-model");
      assert.equal(captured[0].body.stream, true);
      assert.equal(captured[0].body[outputField] ?? captured[0].body.max_tokens, 128);
      const allowed = {
        "openai-completions": ["model", "messages", "stream", "stream_options", "store", "max_tokens", "max_completion_tokens", "temperature", "tools", "tool_choice"],
        "openai-responses": ["model", "input", "stream", "store", "max_output_tokens", "temperature", "tools", "tool_choice"],
        "anthropic-messages": ["model", "messages", "system", "stream", "max_tokens", "temperature", "tools", "tool_choice", "thinking"],
      };
      assert.deepEqual(Object.keys(captured[0].body).filter((key) => !allowed[protocol].includes(key)), [], "original request must stay inside the reviewed native field contract");
      assert.equal(captured[0].body.service_tier, undefined);
      assert.equal(captured[0].body.provider, undefined);
      assert.equal(captured[0].body.fallbacks, undefined);
      assert.equal(captured[0].body.thread, undefined);
      const toolNames = captured[0].body.tools.map((tool) => {
        if (protocol === "openai-completions") { assert.equal(tool.type, "function"); return tool.function.name; }
        if (protocol === "openai-responses") { assert.equal(tool.type, "function"); return tool.name; }
        assert.equal(tool.type, undefined); assert.equal(tool.defer_loading, undefined); return tool.name;
      });
      assert.deepEqual(toolNames.sort(), ["concat_version", "effects_list", "project_get", "template_list", "timeline_read"]);
    } finally { await f.runtime.stop().catch(() => {}); rmSync(directory, { recursive: true, force: true }); }
  }
});

test("verified DeepSeek Flash context metadata is scoped to the exact official HTTPS origin and model", () => {
  const official = {
    ...nativeConfiguration(), endpoint: "https://api.deepseek.com/", model: "deepseek-flash",
    maxOutputTokens: 4096, requestTimeoutMs: 60000,
  };
  const modelOf = (raw) => api.nativeModelConfiguration(raw).providers[0].getModels()[0];
  for (const endpoint of ["https://api.deepseek.com/", "https://api.deepseek.com/v1", "https://api.deepseek.com:443/"]) {
    const configuration = api.nativeModelConfiguration({ ...official, endpoint });
    const model = configuration.providers[0].getModels()[0];
    assert.equal(model.contextWindow, 1048576);
    assert.equal(model.maxTokens, 4096, "service context capacity never raises the user output ceiling");
    assert.equal(configuration.native.maxOutputTokens, 4096);
    assert.equal(configuration.native.requestTimeoutMs, 60000);
    assert.equal(model.reasoning, false, "this fix must not change thinking mode");
    assert.equal(model.compat.thinkingFormat, "openai");
  }
  for (const override of [
    { endpoint: "https://api.deepseek.com.attacker.invalid/" },
    { endpoint: "https://proxy.invalid/api.deepseek.com/" },
    { endpoint: "https://sub.api.deepseek.com/" },
    { endpoint: "https://api.deepseek.com:8443/" },
    { endpoint: "http://127.0.0.1/" },
    { model: "deepseek-v4-flash" },
    { model: "deepseek-chat" },
    { model: "DEEPSEEK-FLASH" },
    { protocol: "openai-responses" },
    { protocol: "anthropic-messages" },
  ]) assert.equal(modelOf({ ...official, ...override }).contextWindow, 32768, JSON.stringify(override));
  assert.equal(modelOf({ ...official, endpoint: "https://proxy.invalid/", maxOutputTokens: 20000 }).contextWindow, 40000,
    "unverified models retain their prior admission behavior");
});

test("original output budgeting no longer clips the verified official model to one token after 29k history", { timeout: 20000 }, async () => {
  // Exercise the actual native model wrapper, original buildBaseOptions and
  // original SDK request construction. The native fetch fixture records only
  // safe request metadata and performs no network or credential access.
  const { build } = await import(pathToFileURL(require.resolve("esbuild")).href);
  const output = await build({
    absWorkingDir: referenceRoot,
    stdin: { contents: [
      `export { nativeModels } from ${JSON.stringify(resolve(packageRoot, "src/models.ts"))};`,
      `export { ExecutionGate } from ${JSON.stringify(resolve(packageRoot, "src/gate.ts"))};`,
      `export { buildBaseOptions } from ${JSON.stringify(resolve(referenceRoot, "packages/ai/src/api/simple-options.ts"))};`,
      `export { estimateContextTokens } from ${JSON.stringify(resolve(referenceRoot, "packages/ai/src/utils/estimate.ts"))};`,
    ].join("\n"), loader: "ts", resolveDir: packageRoot },
    tsconfigRaw: { compilerOptions: { paths: referencePaths() } },
    nodePaths: [resolve(referenceRoot, "node_modules")],
    bundle: true, platform: "browser", target: "es2022", format: "iife", globalName: "PiModelBudgetContract", write: false,
  });
  vm.runInContext(output.outputFiles[0].text, sandbox, { filename: "pinned-model-budget-contract.js" });
  const { nativeModels, ExecutionGate, buildBaseOptions, estimateContextTokens } = sandbox.PiModelBudgetContract;
  const configuration = api.nativeModelConfiguration({
    ...nativeConfiguration(), endpoint: "https://api.deepseek.com/", model: "deepseek-flash",
    maxOutputTokens: 4096, requestTimeoutMs: 60000,
  });
  const lease = { storageId: "model-budget", generation: "1", handle: "model-budget-owner" };
  const gate = new ExecutionGate(); gate.acquiring(); gate.owned(lease);
  gate.activating(lease, { generation: "1", snapshotId: "reviewed", allowInitializeEmptyStore: false }); gate.ready(lease);
  gate.authorizeNetwork(lease, "explicit-model-budget-permit");
  const captured = [];
  const models = nativeModels(configuration, { network: {
    async fetch(_lease, permit, input, init) {
      assert.equal(permit, "explicit-model-budget-permit");
      const request = new Request(input, init);
      const body = JSON.parse(await request.text());
      captured.push({
        maxTokens: body.max_tokens, alternate: body.max_completion_tokens,
        thinking: body.thinking, effort: body.reasoning_effort,
      });
      throw new Error("deterministic local request capture; no service request");
    },
  } }, gate, lease);
  const registered = models.getAllModels().find((model) => model.id === "deepseek-flash");
  assert.equal(registered.contextWindow, 1048576);
  for (const [previousTotal, trailingTokens, contextWindow, expected] of [
    [27560, 401, 32768, 711], [28800, 173, 32768, 1], [28800, 173, registered.contextWindow, 4096],
  ]) {
    const context = nativeJsonReply({ messages: [{
      role: "assistant", content: [{ type: "text", text: "synthetic prior answer" }],
      api: "openai-completions", provider: configuration.native.id, model: "deepseek-flash",
      timestamp: 1, stopReason: "stop",
      usage: { input: previousTotal, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: previousTotal,
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    }, {
      // Reproduce the observed budget totals without copying private text.
      role: "user", content: "x".repeat(Math.floor(trailingTokens * 3.5)), timestamp: 2,
    }] });
    const model = { ...registered, contextWindow };
    assert.equal(estimateContextTokens(context).tokens, previousTotal + trailingTokens);
    assert.equal(buildBaseOptions(model, context, { maxTokens: 4096 }).maxTokens, expected);
    // A caller-proposed larger output budget cannot bypass native settings.
    const before = captured.length;
    const result = await models.streamSimple(model, context, { maxTokens: 393216 }).result();
    assert.equal(result.stopReason, "error", "the explicit capture fixture stops before any service call");
    assert.equal(captured.length, before + 1, "no retry on capture refusal");
    assert.deepEqual(captured.at(-1), { maxTokens: expected, alternate: undefined, thinking: undefined, effort: undefined });
  }
  gate.finishNetwork("explicit-model-budget-permit");
  const denied = await models.streamSimple(registered, nativeJsonReply({ messages: [] }), {}).result();
  assert.equal(denied.stopReason, "error");
  assert.equal(captured.length, 3, "verified context metadata does not grant native network authority");
  gate.stop();
  assert.equal(forbiddenFetch, 0);
});

test("native configuration refuses secrets, unsafe addresses and lossy revisions before opening a provider", () => {
  const valid = nativeConfiguration();
  for (const extra of [
    { apiKey: "not-a-place-for-secrets" }, { endpoint: "https://user:password@model.invalid/" },
    { endpoint: "https://model.invalid/v1?api_key=secret" }, { endpoint: "http://public.invalid" },
    { endpoint: "https://model.invalid/v1?" }, { endpoint: "https://model.invalid/v1#" },
    { endpoint: "https://model.invalid:0/v1" }, { endpoint: " https://model.invalid/v1" },
    { version: 9007199254740992 }, { configurationVersion: "18446744073709551616" },
    { maxOutputTokens: 0 }, { requestTimeoutMs: 0 }, { protocol: "ambient-oauth" },
  ]) assert.throws(() => api.nativeModelConfiguration({ ...valid, ...extra }));
  assert.equal(api.nativeModelConfiguration(valid).pricingKnown, false);
  assert.equal(api.nativeModelConfiguration({ ...valid, endpoint: "http://127.0.0.2/v1" }).native.endpoint, "http://127.0.0.2/v1");
});

test("built production bootstrap submits through native task binding, streams through wire and emits only the original durable answer", { timeout: 15000 }, async () => {
  const directory = mkdtempSync(resolve(tmpdir(), "bkcat-bootstrap-production-"));
  const sqlite = sqliteFixture(resolve(directory, "session.sqlite3"), 1);
  const waiting = new Map(); const listeners = new Set(); const sent = [];
  let detached; let bound = false; let opens = 0; let cursor = 0;
  let settle;
  const completed = new Promise((resolve) => { settle = resolve; });
  const response = await completionResponse([{ delta: { role: "assistant", content: "经生产入口保存的候选" } }, { finish: "stop" }]).text();
  const bytes = new TextEncoder().encode(response);
  const receive = (message) => { for (const listener of [...listeners]) listener({ data: JSON.stringify(message) }); };
  sandbox.addEventListener = (type, listener) => { if (type === "pagehide") detached = listener; };
  sandbox.BKCatNative = {
    addEventListener(_type, listener) { listeners.add(listener); },
    removeEventListener(_type, listener) { listeners.delete(listener); },
    postMessage(serialized) {
      const frame = JSON.parse(serialized); sent.push(frame);
      if (frame.event === "task.completed") settle(frame);
      if (frame.id && !frame.payload) {
        const pending = waiting.get(frame.id);
        if (pending) { waiting.delete(frame.id); if (frame.error) pending.reject(new Error(frame.error.message)); else pending.resolve(frame.result); }
      }
      if (!frame.id || !frame.payload) return;
      void Promise.resolve().then(async () => {
        const payload = frame.payload;
        if (payload.method === "ownership.acquire") return { storageId: "isolated-pi", generation: "1", handle: "owner-1" };
        if (payload.method === "recovery.read") return { snapshotId: "native-reviewed", projectSession: "1", projectRevision: "0", sessionRevision: "0", schemaVersion: "empty", hasInterruptedWork: false };
        if (payload.method === "ownership.confirm") return { generation: "1", snapshotId: "native-reviewed", allowInitializeEmptyStore: true };
        if (payload.method.startsWith("sqlite.")) return sqlite.wire.send(payload);
        if (payload.method === "mcp.send") {
          const message = payload.message;
          if (message.method === "notifications/initialized") return null;
          if (message.method === "initialize") return { jsonrpc: "2.0", id: message.id, result: { protocolVersion: "2025-11-25", capabilities: { tools: {} }, serverInfo: { name: "fixture-native", version: "1" } } };
          if (message.method === "tools/list") return { jsonrpc: "2.0", id: message.id, result: { tools: ["concat_version", "project_get", "timeline_read", "effects_list", "template_list", "edit_apply_guarded"].map((name) => ({ name, inputSchema: { type: "object", properties: {} } })) } };
          throw new Error("unexpected tool call");
        }
        if (payload.method === "task.bind") { assert.equal(payload.taskId, "production-task"); bound = true; return null; }
        if (payload.method === "network.open") {
          assert.equal(bound, true); assert.equal(payload.networkPermit, "production-permit");
          assert.equal(payload.methodName, "POST"); assert.equal(payload.url, "https://model.invalid/custom/chat/completions");
          assert.equal(payload.headers.some(([name]) => ["authorization", "x-api-key", "cookie"].includes(name)), false);
          opens++; return { streamId: "native-stream", status: 200, headers: [["content-type", "text/event-stream"]] };
        }
        if (payload.method === "network.read") {
          const part = bytes.slice(cursor, cursor + 29); cursor += part.length;
          return { chunk: Buffer.from(part).toString("base64"), done: cursor === bytes.length };
        }
        if (payload.method === "network.cancel") return null;
        if (payload.method === "ownership.revoke") { sqlite.revoke(); return null; }
        if (payload.method === "ownership.drainAndRelease") { assert.equal(sqlite.isClosed(), true); return null; }
        throw new Error(`unexpected production frame ${payload.method}`);
      }).then((result) => receive({ channel: "bk-cat-runtime-v1", id: frame.id, result }),
        (error) => receive({ channel: "bk-cat-runtime-v1", id: frame.id, error: { message: error.message } }));
    },
  };
  const command = (id, payload) => new Promise((resolve, reject) => {
    waiting.set(id, { resolve, reject }); receive({ channel: "bk-cat-runtime-v1", id, payload });
  });
  vm.runInContext(readFileSync(resolve(packageRoot, "dist/bootstrap.js"), "utf8"), sandbox, { filename: "bootstrap-production.js" });
  try {
    await command("production-start", { method: "runtime.start", storageId: "isolated-pi", modelConfiguration: nativeConfiguration() });
    await command("production-recovery", { method: "runtime.confirmRecovery", approval: { snapshotId: "native-reviewed", decision: "continue", approvalId: "native-action" } });
    assert.equal(opens, 0, "restoring settings and the original store alone never starts a model request");
    const admitted = await command("production-submit", { method: "runtime.submit", taskId: "production-task", networkPermit: "production-permit", mode: "script", input: "生成一段短视频文案" });
    const final = await completed;
    assert.equal(final.taskId, "production-task"); assert.equal(final.submissionId, admitted.submissionId);
    assert.equal(final.status, "done", final.status === "done" ? undefined : durableFailure(resolve(directory, "session.sqlite3")));
    assert.equal(final.text, "经生产入口保存的候选");
    assert.equal(opens, 1); assert.equal(forbiddenFetch, 0);
    assert.ok(sent.some((frame) => frame.event === "conversation.presentation" && frame.messages.some((message) => message.role === "user" && message.text === "生成一段短视频文案")));
    assert.ok(sent.findIndex((frame) => frame.payload?.method === "task.bind") < sent.findIndex((frame) => frame.payload?.method === "network.open"));
    await command("production-stop", { method: "runtime.stop" });
  } finally {
    await command("production-final-stop", { method: "runtime.stop" }).catch(() => {});
    detached?.(); delete sandbox.BKCatNative;
    rmSync(directory, { recursive: true, force: true });
  }
});
