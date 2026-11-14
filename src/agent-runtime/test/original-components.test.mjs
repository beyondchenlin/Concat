// SPDX-License-Identifier: AGPL-3.0-or-later

// This test executes the built original Pi components with a real isolated
// SQLite file using Node's SQLite driver. Native ownership and MCP are
// explicit fixtures here; it is not a Rust/WebView/device/model acceptance.
import assert from "node:assert/strict";
import { createHash, webcrypto } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
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

test("original pinned McpClient preserves per-call argument identity for the native durable invocation side channel", async () => {
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
  vm.runInContext(output.outputFiles[0].text, sandbox, { filename: "pinned-client-contract.js" });
  const { McpClient, NativeMcpTransport, ExecutionGate } = sandbox.PiTransportContract;
  const lease = { storageId: "contract", generation: "1", handle: "contract-owner" };
  const gate = new ExecutionGate(); gate.acquiring(); gate.owned(lease);
  gate.activating(lease, { generation: "1", snapshotId: "reviewed", allowInitializeEmptyStore: false }); gate.ready(lease);
  const received = [];
  const transport = new NativeMcpTransport({ mcp: {
    async send(_lease, message, _signal, invocation) {
      if (message.method === "initialize") return { jsonrpc: "2.0", id: message.id, result: { protocolVersion: "2025-11-25", capabilities: { tools: {} }, serverInfo: { name: "explicit-fixture", version: "1" } } };
      if (message.method === "notifications/initialized") return undefined;
      assert.equal(message.method, "tools/call");
      assert.equal(message.params.name, "edit_apply_guarded");
      received.push({ arguments: message.params.arguments, invocation });
      return { jsonrpc: "2.0", id: message.id, result: { content: [{ type: "text", text: "Explicit protocol fixture receipt" }], isError: false } };
    },
  } }, lease, gate);
  const client = new McpClient({ name: "original-client-contract", version: "1", protocolVersion: "2025-11-25" });
  try {
    await client.connect(transport);
    const calls = [11, 12].map((taskId) => {
      const args = { expectedSession: 7, expectedRevision: 0, command: { op: "batch", commands: [] } };
      return transport.withInvocation(args, { conversationId: 9, taskId, callId: `stable-${taskId}` }, () => client.callTool("edit_apply_guarded", args));
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
function sqliteFixture(file, generation) {
  let connection;
  let transaction;
  let revoked = false;
  const databaseId = `db-${generation}`;
  const calls = [];
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
  return { calls, wire, revoke: () => { revoked = true; }, isClosed: () => !connection };
}

function fixture(file, generation, configured, transport) {
  const sqlite = sqliteFixture(file, generation);
  const mcpCalls = [];
  let granted = false;
  let networkCalls = 0;
  let credentialReads = 0;
  let snapshotReads = 0;
  const taskBindings = [];
  const lease = { storageId: "isolated-pi", generation: String(generation), handle: `owner-${generation}` };
  const snapshot = () => ({
    snapshotId: `reconciled-${generation}`, projectSession: "1", projectRevision: "0", sessionRevision: "0",
    schemaVersion: existsSync(file) ? 2 : "empty", hasInterruptedWork: false,
  });
  const ports = {
    ownership: {
      async acquire(id) { assert.equal(id, lease.storageId); return lease; },
      async confirm(_lease, read, approval) {
        assert.equal(approval.snapshotId, read.snapshotId); granted = true;
        return { generation: lease.generation, snapshotId: read.snapshotId, allowInitializeEmptyStore: read.schemaVersion === "empty" };
      },
      async revoke() { sqlite.revoke(); },
      async drainAndRelease() { assert.equal(sqlite.isClosed(), true, "SQLite closes before ownership release"); },
      onRevoked() { return () => {}; },
    },
    recovery: { async read() { snapshotReads++; return snapshot(); } },
    sqlite: { async open(...args) { assert.equal(granted, true); return api.nativeSqlitePort(sqlite.wire).open(...args); } },
    mcp: {
      async send(_lease, message, _signal, invocation) {
        mcpCalls.push(message);
        if (message.method === "initialize") {
          assert.equal(message.params.protocolVersion, "2025-11-25");
          return nativeJsonReply({ jsonrpc: "2.0", id: message.id, result: { protocolVersion: "2025-11-25", serverInfo: { name: "explicit-fixture", version: "1" }, capabilities: { tools: {} } } });
        }
        if (message.method === "notifications/initialized") return undefined;
        // The real NativeWire receives JSON in the web realm. Preserve that
        // boundary even in this direct-port fixture; host-realm arrays are
        // rejected by the original durable strict-JSON checks.
        if (message.method === "tools/list") return nativeJsonReply({ jsonrpc: "2.0", id: message.id, result: { tools: [
          "concat_version", "project_get", "timeline_read", "edit_apply_guarded",
        ].map((name) => ({ name, description: "Protocol fixture only", inputSchema: { type: "object", properties: {} } })) } });
        if (transport && message.method === "tools/call") {
          assert.equal(message.params.name, "project_get", "only the fixture's real read path is implemented");
          assert.ok(taskBindings.some((binding) => binding.conversationId === invocation.conversationId));
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
  return { runtime, sqlite, mcpCalls, taskBindings, snapshotReads: () => snapshotReads, networkCalls: () => networkCalls, credentialReads: () => credentialReads };
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
      assert.deepEqual(toolNames.sort(), ["concat_version", "project_get", "timeline_read"]);
    } finally { await f.runtime.stop().catch(() => {}); rmSync(directory, { recursive: true, force: true }); }
  }
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
          if (message.method === "tools/list") return { jsonrpc: "2.0", id: message.id, result: { tools: ["concat_version", "project_get", "timeline_read", "edit_apply_guarded"].map((name) => ({ name, inputSchema: { type: "object", properties: {} } })) } };
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
