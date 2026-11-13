// SPDX-License-Identifier: AGPL-3.0-or-later

import type { SqliteDatabase, SqliteExecutor, SqliteValue } from "@earendil-works/chord/sqlite";
import { RuntimeRefused } from "./gate.ts";
import type { NativePorts, OwnerLease } from "./ports.ts";

export type SqliteWireValue = null | number | string
  | { readonly type: "integer"; readonly value: string }
  | { readonly type: "blob"; readonly base64: string };

export interface SqliteFrame {
  readonly method: "sqlite.open" | "sqlite.begin" | "sqlite.exec" | "sqlite.run" | "sqlite.runCounted"
    | "sqlite.get" | "sqlite.all" | "sqlite.commit" | "sqlite.rollback" | "sqlite.close";
  readonly ownerHandle: string;
  readonly generation: string;
  readonly databaseId?: string;
  readonly transactionId?: string;
  readonly expectedSchemaVersion?: number;
  readonly allowInitializeEmptyStore?: boolean;
  readonly sql?: string;
  readonly params?: readonly SqliteWireValue[];
}

/** Native code returns a value or rejects; it validates the opaque owner,
 * statement, transaction and generation at the actual SQLite boundary.
 * No retry is permitted after an ambiguous result. A missing signal is
 * used only for rollback/close, so revocation cannot prevent cleanup. */
export interface SqliteWire {
  send(frame: SqliteFrame, signal?: AbortSignal): Promise<unknown>;
}

/** Portable original Pi database interface over the actual native port. */
export function nativeSqlitePort(wire: SqliteWire): NativePorts["sqlite"] {
  return {
    async open(lease, options) {
      options.signal.throwIfAborted();
      if (options.expectedSchemaVersion !== 2) throw new RuntimeRefused("unreviewed Pi SQLite schema version");
      const reply = await wire.send({
        method: "sqlite.open", ownerHandle: lease.handle, generation: lease.generation,
        expectedSchemaVersion: options.expectedSchemaVersion,
        allowInitializeEmptyStore: options.allowInitializeEmptyStore,
      }, options.signal);
      const databaseId = identity(reply, "databaseId");
      const database = new NativeDatabase(wire, Object.freeze({ ...lease }), databaseId, options.signal);
      if (options.signal.aborted) {
        await database.close();
        options.signal.throwIfAborted();
      }
      return database;
    },
  };
}

class NativeDatabase implements SqliteDatabase {
  readonly #wire: SqliteWire;
  readonly #lease: OwnerLease;
  readonly #databaseId: string;
  readonly #signal: AbortSignal;
  #tail: Promise<unknown> = Promise.resolve();
  #closing: Promise<void> | undefined;

  constructor(wire: SqliteWire, lease: OwnerLease, databaseId: string, signal: AbortSignal) {
    this.#wire = wire;
    this.#lease = lease;
    this.#databaseId = databaseId;
    this.#signal = signal;
  }

  exec(sql: string): Promise<void> { return this.#enqueue(async () => { await this.#statement("sqlite.exec", sql); }); }
  run(sql: string, ...params: SqliteValue[]): Promise<void> { return this.#enqueue(async () => { await this.#statement("sqlite.run", sql, params); }); }
  runCounted(sql: string, ...params: SqliteValue[]): Promise<number> {
    return this.#enqueue(async () => count(await this.#statement("sqlite.runCounted", sql, params)));
  }
  get<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T | undefined> {
    return this.#enqueue(async () => row<T>(await this.#statement("sqlite.get", sql, params)));
  }
  all<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T[]> {
    return this.#enqueue(async () => rows<T>(await this.#statement("sqlite.all", sql, params)));
  }

  transaction<T>(callback: (transaction: SqliteExecutor) => Promise<T>): Promise<T> {
    return this.#enqueue(async () => {
      const transactionId = identity(await this.#request("sqlite.begin"), "transactionId");
      let accepting = true;
      let statements: Promise<unknown> = Promise.resolve();
      let failed = false;
      let statementFailure: unknown;
      const admitted = <R>(action: () => Promise<R>): Promise<R> => {
        if (!accepting) return Promise.reject(new RuntimeRefused("SQLite transaction callback has ended"));
        const operation = statements.then(action);
        statements = operation.catch((error: unknown) => {
          if (!failed) statementFailure = error;
          failed = true;
        });
        return operation;
      };
      const tx: SqliteExecutor = {
        exec: (sql) => admitted(async () => { await this.#statement("sqlite.exec", sql, undefined, transactionId); }),
        run: (sql, ...params) => admitted(async () => { await this.#statement("sqlite.run", sql, params, transactionId); }),
        runCounted: (sql, ...params) => admitted(async () => count(await this.#statement("sqlite.runCounted", sql, params, transactionId))),
        get: <R extends object>(sql: string, ...params: SqliteValue[]) => admitted(async () => row<R>(await this.#statement("sqlite.get", sql, params, transactionId))),
        all: <R extends object>(sql: string, ...params: SqliteValue[]) => admitted(async () => rows<R>(await this.#statement("sqlite.all", sql, params, transactionId))),
      };
      try {
        const value = await callback(tx);
        accepting = false;
        await statements;
        if (failed) throw statementFailure;
        await this.#request("sqlite.commit", { transactionId });
        return value;
      } catch (error) {
        accepting = false;
        await statements;
        try { await this.#request("sqlite.rollback", { transactionId }, true); }
        catch (rollback) { throw new AggregateError([error, rollback], "SQLite transaction and rollback failed; native reconciliation is required"); }
        throw error;
      }
    });
  }

  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    const close = async () => { await this.#request("sqlite.close", {}, true); };
    this.#closing = this.#tail.then(close, close);
    this.#tail = this.#closing.catch(() => {});
    return this.#closing;
  }

  #enqueue<T>(operation: () => Promise<T>): Promise<T> {
    if (this.#closing) return Promise.reject(new RuntimeRefused("SQLite database is closing"));
    // This is a connection exclusion line, not an agent/task scheduler.
    const result = this.#tail.then(operation);
    this.#tail = result.catch(() => {});
    return result;
  }

  #statement(method: SqliteFrame["method"], sql: string, params?: readonly SqliteValue[], transactionId?: string): Promise<unknown> {
    if (typeof sql !== "string" || !sql.trim()) return Promise.reject(new RuntimeRefused("SQLite statement is empty"));
    return this.#request(method, { sql, ...(params ? { params: params.map(encodeSqliteValue) } : {}), ...(transactionId ? { transactionId } : {}) });
  }

  async #request(method: SqliteFrame["method"], fields: Partial<SqliteFrame> = {}, cleanup = false): Promise<unknown> {
    if (!cleanup) this.#signal.throwIfAborted();
    const result = await this.#wire.send({
      ...fields, method, ownerHandle: this.#lease.handle, generation: this.#lease.generation, databaseId: this.#databaseId,
    }, cleanup ? undefined : this.#signal);
    if (!cleanup) this.#signal.throwIfAborted();
    return result;
  }
}

export function encodeSqliteValue(value: SqliteValue): SqliteWireValue {
  if (value === null || typeof value === "string") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value))) {
      throw new RuntimeRefused("SQLite numeric argument loses precision; use bigint for 64-bit integers");
    }
    return value;
  }
  if (typeof value === "bigint") {
    if (value < -(1n << 63n) || value > (1n << 63n) - 1n) throw new RuntimeRefused("SQLite integer is outside signed 64-bit range");
    return { type: "integer", value: value.toString() };
  }
  if (value instanceof Uint8Array) {
    let binary = "";
    for (let offset = 0; offset < value.length; offset += 8192) binary += String.fromCharCode(...value.subarray(offset, offset + 8192));
    return { type: "blob", base64: btoa(binary) };
  }
  throw new RuntimeRefused("unsupported SQLite parameter type");
}

export function decodeSqliteValue(value: unknown): SqliteValue {
  if (value === null || typeof value === "string") return value;
  if (typeof value === "number") return encodeSqliteValue(value) as number;
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new RuntimeRefused("invalid SQLite wire value");
  const tagged = value as { type?: unknown; value?: unknown; base64?: unknown };
  if (tagged.type === "integer" && typeof tagged.value === "string" && /^-?(0|[1-9][0-9]*)$/.test(tagged.value)) {
    const result = BigInt(tagged.value);
    encodeSqliteValue(result);
    return result;
  }
  if (tagged.type === "blob" && typeof tagged.base64 === "string") {
    const binary = atob(tagged.base64);
    if (btoa(binary) !== tagged.base64) throw new RuntimeRefused("SQLite blob uses noncanonical base64");
    return Uint8Array.from(binary, (character) => character.charCodeAt(0));
  }
  throw new RuntimeRefused("invalid tagged SQLite wire value");
}

function identity(value: unknown, key: "databaseId" | "transactionId"): string {
  if (!value || typeof value !== "object" || typeof (value as Record<string, unknown>)[key] !== "string" || !(value as Record<string, unknown>)[key]) {
    throw new RuntimeRefused(`native SQLite omitted ${key}`);
  }
  return (value as Record<string, string>)[key]!;
}
function count(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new RuntimeRefused("native SQLite returned an invalid change count");
  return value;
}
function row<T extends object>(value: unknown): T | undefined {
  if (value === null) return undefined;
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new RuntimeRefused("native SQLite returned an invalid row");
  return Object.fromEntries(Object.entries(value).map(([key, field]) => [key, decodeSqliteValue(field)])) as T;
}
function rows<T extends object>(value: unknown): T[] {
  if (!Array.isArray(value)) throw new RuntimeRefused("native SQLite returned invalid rows");
  return value.map((entry) => {
    const decoded = row<T>(entry);
    if (decoded === undefined) throw new RuntimeRefused("native SQLite returned an absent row inside a result set");
    return decoded;
  });
}
