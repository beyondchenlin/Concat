// SPDX-License-Identifier: AGPL-3.0-or-later

import type { SqliteDatabase, SqliteExecutor } from "@earendil-works/chord/sqlite";
import type { Storage } from "@earendil-works/pi-durable";
import type {
  JsonRpcMessage, McpTransport, McpTransportCloseListener, McpTransportErrorListener,
  McpTransportMessageListener, McpTransportSendOptions,
} from "@earendil-works/pi-mcp";
import { combinedSignal, ExecutionGate, RuntimeRefused } from "./gate.ts";
import type { InvocationContext, NativePorts, OwnerLease } from "./ports.ts";

export const EMBEDDED_TOOLS = Object.freeze([
  "concat_version", "project_get", "timeline_read", "edit_apply_guarded",
] as const);

/** Original Pi's portable SQLite driver facade, fenced at every call. */
export function guardedDatabase(db: SqliteDatabase, gate: ExecutionGate, lease: OwnerLease): SqliteDatabase {
  const executor = (target: SqliteExecutor): SqliteExecutor => ({
    async exec(sql) {
      gate.assertStorageWrite(lease);
      await target.exec(sql);
      gate.assertStorageWrite(lease);
    },
    async run(sql, ...params) {
      gate.assertStorageWrite(lease);
      await target.run(sql, ...params);
      gate.assertStorageWrite(lease);
    },
    async runCounted(sql, ...params) {
      gate.assertStorageWrite(lease);
      const result = await target.runCounted(sql, ...params);
      gate.assertStorageWrite(lease);
      return result;
    },
    async get<T extends object>(sql: string, ...params: Parameters<SqliteExecutor["run"]> extends [string, ...infer P] ? P : never) {
      gate.assertOwned(lease);
      const result = await target.get<T>(sql, ...params);
      gate.assertOwned(lease);
      return result;
    },
    async all<T extends object>(sql: string, ...params: Parameters<SqliteExecutor["run"]> extends [string, ...infer P] ? P : never) {
      gate.assertOwned(lease);
      const result = await target.all<T>(sql, ...params);
      gate.assertOwned(lease);
      return result;
    },
  });
  return {
    ...executor(db),
    async transaction<T>(callback: (transaction: SqliteExecutor) => Promise<T>): Promise<T> {
      gate.assertStorageWrite(lease);
      const result = await db.transaction(async (tx) => {
        gate.assertStorageWrite(lease);
        const value = await callback(executor(tx));
        gate.assertStorageWrite(lease);
        return value;
      });
      gate.assertStorageWrite(lease);
      return result;
    },
    // Resource cleanup is allowed after revocation. The native handle
    // belongs only to this lease; closing it must not touch a new owner.
    close: () => db.close(),
  };
}

/** Also fence memory-only id minting and late commit/publication callbacks. */
export function guardedStorage(storage: Storage, gate: ExecutionGate, lease: OwnerLease): Storage {
  const reads = new Set([
    "conversation", "scanConversations", "entry", "findLatestHeadMarker", "scanEntries", "task", "scanTasks",
    "submission", "scanSubmissions", "submissionByRequest", "findDocument", "document", "scanDocuments",
  ]);
  return new Proxy(storage, {
    get(target, property) {
      if (property === "close") return target.close.bind(target);
      if (property === "commit") return async (...args: Parameters<Storage["commit"]>) => {
        gate.assertStorageWrite(lease);
        const receive = args[1];
        const result = await target.commit(args[0], (index, seq) => {
          gate.assertStorageWrite(lease);
          receive(index, seq);
        }, args[2]);
        gate.assertStorageWrite(lease);
        return result;
      };
      if (property === "mintId") return async () => {
        gate.assertStorageWrite(lease);
        const id = await target.mintId();
        gate.assertStorageWrite(lease);
        return id;
      };
      if (property === "publishing") return target.publishing ? (publish: () => void, context: Parameters<NonNullable<Storage["publishing"]>>[1]) => {
        gate.assertStorageWrite(lease);
        target.publishing!(() => { gate.assertStorageWrite(lease); publish(); }, context);
      } : undefined;
      if (typeof property === "string" && reads.has(property)) {
        const read = Reflect.get(target, property) as (...args: unknown[]) => Promise<unknown>;
        return async (...args: unknown[]) => {
          gate.assertOwned(lease);
          const value = await Reflect.apply(read, target, args);
          gate.assertOwned(lease);
          return value;
        };
      }
      return undefined;
    },
  });
}

/** A transport for the original McpClient, not a second protocol client. */
export class NativeMcpTransport implements McpTransport {
  readonly #ports: NativePorts;
  readonly #lease: OwnerLease;
  readonly #gate: ExecutionGate;
  readonly #messages = new Set<McpTransportMessageListener>();
  readonly #errors = new Set<McpTransportErrorListener>();
  readonly #closedListeners = new Set<McpTransportCloseListener>();
  readonly #invocations = new WeakMap<object, InvocationContext>();
  #started = false;
  #closed = false;

  constructor(ports: NativePorts, lease: OwnerLease, gate: ExecutionGate) {
    this.#ports = ports;
    this.#lease = lease;
    this.#gate = gate;
  }

  async start(): Promise<void> {
    this.#gate.assertOwned(this.#lease);
    if (this.#closed || this.#started) throw new RuntimeRefused("MCP transport cannot restart");
    this.#started = true;
  }

  setProtocolVersion(version: string): void {
    if (version !== "2025-11-25") throw new RuntimeRefused("native MCP did not negotiate the pinned Pi protocol");
  }

  /** The original McpClient retains the caller's arguments object in its
   * tools/call frame. Bind that identity out of band, so JSON arguments or
   * model-supplied _meta can never forge the durable operation identity. */
  async withInvocation<T>(argumentsObject: Record<string, unknown>, invocation: InvocationContext, call: () => Promise<T>): Promise<T> {
    this.#gate.assertReady(this.#lease);
    if (!Number.isSafeInteger(invocation.conversationId) || invocation.conversationId < 0
      || !Number.isSafeInteger(invocation.taskId) || invocation.taskId < 0
      || typeof invocation.callId !== "string" || !invocation.callId) {
      throw new RuntimeRefused("original Pi tool invocation identity is invalid");
    }
    if (this.#invocations.has(argumentsObject)) throw new RuntimeRefused("tool argument identity is already admitted");
    this.#invocations.set(argumentsObject, Object.freeze({ ...invocation }));
    try { return await call(); }
    finally { this.#invocations.delete(argumentsObject); }
  }

  async send(message: JsonRpcMessage, options: McpTransportSendOptions = {}): Promise<void> {
    this.#gate.assertOwned(this.#lease);
    if (!this.#started || this.#closed) throw new RuntimeRefused("MCP transport is closed");
    const frame = message as { method?: string; params?: { name?: string; arguments?: unknown } };
    const argumentsObject = frame.params?.arguments;
    const invocation = argumentsObject && typeof argumentsObject === "object" ? this.#invocations.get(argumentsObject) : undefined;
    if (frame.method === "tools/call") {
      if (!(EMBEDDED_TOOLS as readonly string[]).includes(frame.params?.name ?? "")) {
        throw new RuntimeRefused("unreviewed native MCP tool");
      }
      if (frame.params?.name === "edit_apply_guarded") {
        this.#gate.assertReady(this.#lease);
        if (!invocation) throw new RuntimeRefused("guarded edits require the original Pi invocation context");
      }
    } else if (!["initialize", "notifications/initialized", "notifications/cancelled", "tools/list", "ping"].includes(frame.method ?? "")) {
      throw new RuntimeRefused("unreviewed native MCP method");
    }
    const signal = combinedSignal(this.#gate.signal, options.signal);
    signal.throwIfAborted();
    try {
      const reply = await this.#ports.mcp.send(this.#lease, message, signal, invocation);
      this.#gate.assertOwned(this.#lease);
      signal.throwIfAborted();
      if (this.#closed) throw new RuntimeRefused("MCP reply arrived after transport close");
      if (reply !== undefined) {
        if (!reply || typeof reply !== "object" || (reply as { jsonrpc?: unknown }).jsonrpc !== "2.0") {
          throw new RuntimeRefused("native MCP reply is not a JSON-RPC 2.0 object");
        }
        for (const listener of this.#messages) listener(reply as JsonRpcMessage);
      }
    } catch (error) {
      if (!this.#closed) for (const listener of this.#errors) listener(error instanceof Error ? error : new Error(String(error)));
      throw error;
    }
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    for (const listener of this.#closedListeners) listener();
    this.#messages.clear();
    this.#errors.clear();
    this.#closedListeners.clear();
    // Native ownership and native Session cleanup belong to the host's
    // revoke/drain path, not to a client reconnect or transport close.
  }

  onMessage(listener: McpTransportMessageListener) { this.#messages.add(listener); return () => this.#messages.delete(listener); }
  onError(listener: McpTransportErrorListener) { this.#errors.add(listener); return () => this.#errors.delete(listener); }
  onClose(listener: McpTransportCloseListener) { this.#closedListeners.add(listener); return () => this.#closedListeners.delete(listener); }
}
