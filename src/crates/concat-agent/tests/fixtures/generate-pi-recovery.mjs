// SPDX-License-Identifier: AGPL-3.0-or-later
// Generate the regression database through unmodified pinned Pi storage. Never
// use this script on a user database: the output file must not already exist.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const out = resolve(process.argv[2] ?? here);
const referenceScript = resolve(process.argv[3] ?? resolve(here, "../../../../agent-runtime/scripts/reference.mjs"));
const { pins, referencePaths, referenceRoot, verifyReference } = await import(pathToFileURL(referenceScript).href);
const require = verifyReference();
assert.equal(pins.packageVersion, "1.5.2");
assert.equal(pins.commit, "d36266e06669c6991a7c9509d1da04d18d30378d");
const { build } = await import(pathToFileURL(require.resolve("esbuild")).href);
mkdirSync(out, { recursive: true });
const database = resolve(out, "pi-recovery-1.5.2.sqlite3");
assert.equal(existsSync(database), false, "do not overwrite an existing fixture or historical database");
const bundle = resolve(out, "pi-recovery-storage.mjs");
await build({
  absWorkingDir: referenceRoot,
  stdin: { contents: `export { openNodeSqliteStorage } from ${JSON.stringify(resolve(referenceRoot, "packages/durable/src/storage/sqlite/node.ts"))};`, loader: "ts", resolveDir: referenceRoot },
  tsconfigRaw: { compilerOptions: { paths: referencePaths() } },
  nodePaths: [resolve(referenceRoot, "node_modules")], bundle: true,
  platform: "node", target: "node24", format: "esm", outfile: bundle,
});
const { openNodeSqliteStorage } = await import(pathToFileURL(bundle).href);
const storage = await openNodeSqliteStorage(database);
const writes = [{ type: "conversation", value: { id: 7 } }];
const add = (type, value) => writes.push({ type, value });
function submission(id, entry, status) {
  add("submission", { id, conversationId: 7, type: "input", requestId: `native-${id}`, entry, status });
  add("entry", { id: entry, conversationId: 7, kind: "bk-cat.user", data: { text: `Synthetic request ${id}` }, model: [] });
}
function tool(id, assistant, owner) {
  add("task", { id: owner, conversationId: 7, kind: "pi.generation", version: 3,
    input: {}, state: { status: "terminal", outcome: { status: "completed" } } });
  add("entry", { id: assistant, conversationId: 7, kind: "pi.assistant", byTaskId: owner,
    data: { message: { role: "assistant", content: [{ type: "toolCall", id: `call-${id}`, name: "synthetic_test_tool", arguments: {} }] } }, model: [] });
  add("task", { id, conversationId: 7, kind: "pi.tool", version: 4, owner,
    input: { kind: "model", assistant, callId: `call-${id}` },
    state: { status: "terminal", outcome: { status: "completed" } } });
}
submission(1, 2, "done"); tool(6, 5, 4);
submission(10, 11, "placed"); tool(15, 14, 13); tool(19, 18, 17);
submission(25, 26, "done"); tool(30, 29, 28);
try {
  const result = await storage.commit([writes], () => {}, undefined);
  assert.equal(result.stored, 1);
  assert.equal(result.refused, undefined);
} finally { await storage.close(); }
const db = new DatabaseSync(database, { readOnly: true });
let tasks;
try {
  tasks = db.prepare("SELECT id,kind,record FROM tasks ORDER BY id").all().map(({ id, kind, record }) => ({ id, indexedKind: kind, recordKind: JSON.parse(record).kind }));
  assert.deepEqual(tasks.filter(row => row.recordKind === "pi.tool").map(row => row.indexedKind), Array(4).fill('"pi.tool"'));
} finally { db.close(); }
const manifest = { piVersion: pins.packageVersion, piCommit: pins.commit,
  generatorSha256: createHash("sha256").update(readFileSync(fileURLToPath(import.meta.url))).digest("hex"),
  databaseSha256: createHash("sha256").update(readFileSync(database)).digest("hex"),
  databaseBytes: readFileSync(database).length,
  createdBy: "unmodified pinned openNodeSqliteStorage + SqliteStorage.commit",
  sourceRowsSha256: createHash("sha256").update(readFileSync(resolve(referenceRoot, "packages/durable/src/storage/sqlite/rows.ts"))).digest("hex"),
  sourceCommitSha256: createHash("sha256").update(readFileSync(resolve(referenceRoot, "packages/durable/src/storage/sqlite/commit.ts"))).digest("hex"),
  externalModelCalls: 0, userDataIncluded: false, taskIndexes: tasks,
};
writeFileSync(resolve(out, "pi-recovery-1.5.2.json"), JSON.stringify(manifest, null, 2) + "\n");
console.log(JSON.stringify(manifest));
