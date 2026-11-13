// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { decodeSqliteValue, encodeSqliteValue, nativeSqlitePort, type SqliteFrame } from "../src/sqlite-port.ts";

const lease = { storageId: "test", handle: "owned-handle", generation: "9" };
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>((done) => { resolve = done; }); return { promise, resolve }; }
async function fixture() {
  const frames: SqliteFrame[] = [];
  const abort = new AbortController();
  let respond: (frame: SqliteFrame) => Promise<unknown> = async (frame) => {
    if (frame.method === "sqlite.open") return { databaseId: "database-9" };
    if (frame.method === "sqlite.begin") return { transactionId: "transaction-9" };
    if (frame.method === "sqlite.get") return null;
    if (frame.method === "sqlite.all") return [];
    if (frame.method === "sqlite.runCounted") return 2;
    return null;
  };
  const port = nativeSqlitePort({ async send(frame, signal) { signal?.throwIfAborted(); frames.push(frame); return respond(frame); } });
  const database = await port.open(lease, { expectedSchemaVersion: 2, allowInitializeEmptyStore: true, signal: abort.signal });
  return { database, frames, abort, setRespond: (fn: typeof respond) => { respond = fn; } };
}

test("SQLite values preserve signed 64-bit integers and bytes without lossy numeric coercion", () => {
  for (const value of [0n, -(1n << 63n), (1n << 63n) - 1n, new Uint8Array([0, 128, 255]), null, 1.5, "text"]) {
    assert.deepEqual(decodeSqliteValue(encodeSqliteValue(value)), value);
  }
  for (const value of [NaN, Infinity, 9007199254740992, 1n << 63n]) assert.throws(() => encodeSqliteValue(value));
  for (const value of [{ type: "integer", value: "01" }, { type: "integer", value: "9223372036854775808" }, { type: "blob", base64: "AA" }, {}]) {
    assert.throws(() => decodeSqliteValue(value));
  }
});

test("SQLite connection queues outside work until its transaction commits; expired transaction handles refuse", async () => {
  const f = await fixture();
  const entered = deferred<void>(); const release = deferred<void>();
  let oldTransaction: Parameters<typeof f.database.transaction>[0] extends (tx: infer T) => unknown ? T : never;
  const tx = f.database.transaction(async (transaction) => {
    oldTransaction = transaction;
    await transaction.run("INSERT 1", 4n);
    entered.resolve();
    await release.promise;
    return 7;
  });
  await entered.promise;
  const outside = f.database.run("INSERT 2");
  assert.equal(f.frames.some((frame) => frame.sql === "INSERT 2"), false);
  release.resolve();
  assert.equal(await tx, 7); await outside;
  assert.deepEqual(f.frames.map((frame) => frame.method), ["sqlite.open", "sqlite.begin", "sqlite.run", "sqlite.commit", "sqlite.run"]);
  await assert.rejects(oldTransaction!.run("INSERT expired"));
  for (const frame of f.frames) { assert.equal(frame.generation, "9"); assert.equal(frame.ownerHandle, "owned-handle"); }
  assert.equal(f.frames[2]?.transactionId, "transaction-9");
  assert.equal(f.frames[4]?.transactionId, undefined);
  await f.database.close();
});

test("SQLite callback failure rolls back; abort still permits rollback and close cleanup", async () => {
  const f = await fixture();
  await assert.rejects(f.database.transaction(async (tx) => { await tx.run("INSERT"); f.abort.abort(); throw new Error("cancel"); }), /cancel/);
  assert.equal(f.frames.at(-1)?.method, "sqlite.rollback");
  await assert.rejects(f.database.run("LATE"));
  await f.database.close();
  assert.equal(f.frames.at(-1)?.method, "sqlite.close");
  assert.equal(f.frames.some((frame) => frame.method === "sqlite.commit"), false);
});

test("SQLite swallowed statement failure cannot commit and rollback failure remains explicit", async () => {
  const f = await fixture();
  f.setRespond(async (frame) => {
    if (frame.method === "sqlite.begin") return { transactionId: "tx" };
    if (frame.method === "sqlite.run") throw undefined;
    if (frame.method === "sqlite.rollback") throw new Error("rollback lost");
    return null;
  });
  await assert.rejects(f.database.transaction(async (tx) => { await tx.run("FAIL").catch(() => {}); }), AggregateError);
  assert.equal(f.frames.some((frame) => frame.method === "sqlite.commit"), false);
  assert.equal(f.frames.at(-1)?.method, "sqlite.rollback");
  await f.database.close();
});

test("SQLite wire absent rows, tagged result values and affected counts conform to the original interface", async () => {
  const f = await fixture();
  assert.equal(await f.database.get("SELECT absent"), undefined);
  assert.equal(await f.database.runCounted("UPDATE"), 2);
  f.setRespond(async (frame) => frame.method === "sqlite.all" ? [{ id: 2, exact: { type: "integer", value: "9223372036854775807" }, data: { type: "blob", base64: "AP8=" } }] : null);
  assert.deepEqual(await f.database.all("SELECT"), [{ id: 2, exact: 9223372036854775807n, data: new Uint8Array([0, 255]) }]);
  await f.database.close();
  await assert.rejects(f.database.all("AFTER CLOSE"));
});
