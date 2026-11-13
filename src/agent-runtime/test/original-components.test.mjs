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
  Request, Response, Headers, TextDecoder, TextEncoder, Uint8Array,
  setTimeout, clearTimeout, queueMicrotask, performance, crypto: webcrypto, btoa, atob,
  fetch: () => { forbiddenFetch++; throw new Error("Direct browser network is forbidden in this test"); },
});
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
  assert.ok(sent.some((message) => message.event === "runtime.loaded" && message.modelTransport === "unconfigured"));
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

function fixture(file, generation) {
  const sqlite = sqliteFixture(file, generation);
  const mcpCalls = [];
  let granted = false;
  let networkCalls = 0;
  let credentialReads = 0;
  let snapshotReads = 0;
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
      async send(_lease, message) {
        mcpCalls.push(message);
        if (message.method === "initialize") {
          assert.equal(message.params.protocolVersion, "2025-11-25");
          return { jsonrpc: "2.0", id: message.id, result: { protocolVersion: "2025-11-25", serverInfo: { name: "explicit-fixture", version: "1" }, capabilities: { tools: {} } } };
        }
        if (message.method === "notifications/initialized") return undefined;
        if (message.method === "tools/list") return { jsonrpc: "2.0", id: message.id, result: { tools: [
          "concat_version", "project_get", "timeline_read", "edit_apply_guarded",
        ].map((name) => ({ name, description: "Protocol fixture only", inputSchema: { type: "object", properties: {} } })) } };
        throw new Error(`Unexpected model/native tool call ${message.method}`);
      },
    },
    network: { async fetch() { networkCalls++; throw new Error("No model service is authorized"); } },
  };
  const configuration = { providers: [], credentials: {
    async read() { credentialReads++; throw new Error("No credentials are configured"); },
    async list() { throw new Error("No credential enumeration"); },
    async modify() { throw new Error("No credential modification"); },
    async delete() { throw new Error("No credential deletion"); },
  } };
  const runtime = new api.AgentRuntime(lease.storageId, ports, api.piEngineFactory(configuration));
  return { runtime, sqlite, mcpCalls, snapshotReads: () => snapshotReads, networkCalls: () => networkCalls, credentialReads: () => credentialReads };
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
