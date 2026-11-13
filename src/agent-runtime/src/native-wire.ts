// SPDX-License-Identifier: AGPL-3.0-or-later

import { RuntimeRefused } from "./gate.ts";
import { nativeSqlitePort } from "./sqlite-port.ts";
import type { NativePorts, OwnerLease, RecoverySnapshot, ExecutionGrant } from "./ports.ts";

export const NATIVE_CHANNEL = "bk-cat-runtime-v1";

/** Installed by the native /transport.js resource for this component.
 * It adds an unguessable component handle/generation to every frame. */
export interface NativeMessageBridge {
  postMessage(serializedEnvelope: string): void;
  addEventListener(type: "message", listener: (event: { data: string }) => void): void;
  removeEventListener(type: "message", listener: (event: { data: string }) => void): void;
}

interface Pending {
  resolve(value: unknown): void;
  reject(error: unknown): void;
  detach(): void;
}

/** Correlates one-way native messages; it has no retry or task scheduler. */
export class NativeWireClient {
  readonly #bridge: NativeMessageBridge;
  readonly #prefix = crypto.randomUUID();
  readonly #pending = new Map<string, Pending>();
  readonly #revocations = new Set<(lease: Pick<OwnerLease, "handle" | "generation">) => void>();
  readonly #revoked = new Set<string>();
  readonly #savedAcks = new Set<string>();
  #owner: OwnerLease | undefined;
  #sequence = 0;
  #closed = false;
  readonly #receive = (event: { data: string }) => {
    let envelope: Record<string, unknown>;
    try { envelope = record(JSON.parse(event.data)); } catch { return; }
    if (envelope.channel !== NATIVE_CHANNEL) return;
    if (envelope.event === "owner.revoked" && typeof envelope.ownerHandle === "string" && typeof envelope.generation === "string") {
      this.#revoked.add(JSON.stringify([envelope.ownerHandle, envelope.generation]));
      for (const listener of this.#revocations) listener({ handle: envelope.ownerHandle, generation: envelope.generation });
      return;
    }
    if (envelope.event === "operation.saved") {
      const owner = this.#owner;
      const operationId = envelope.operationId;
      if (!owner || this.#closed || envelope.ownerHandle !== owner.handle || envelope.generation !== owner.generation
        || this.#revoked.has(JSON.stringify([owner.handle, owner.generation]))
        || typeof operationId !== "string" || !/^[1-9][0-9]*$/.test(operationId) || operationId.length > 20) return;
      if (this.#savedAcks.has(operationId)) return;
      if (this.#savedAcks.size >= 65536) { this.#savedFailure("native save acknowledgement limit reached"); return; }
      this.#savedAcks.add(operationId);
      // A native save request is not a save acknowledgement. Only this actual
      // native Written notification reaches here, and only receipt by this
      // live component produces the final native journal acknowledgement.
      void this.call({ method: "operation.acknowledge", operationId,
        ownerHandle: owner.handle, generation: owner.generation,
      }).catch((error: unknown) => this.#savedFailure(error instanceof Error ? error.message : String(error)));
      return;
    }
    if (typeof envelope.id !== "string" || ("result" in envelope) === ("error" in envelope)) return;
    const pending = this.#pending.get(envelope.id);
    if (!pending) return;
    this.#pending.delete(envelope.id);
    pending.detach();
    if ("error" in envelope) {
      const error = envelope.error;
      const message = error && typeof error === "object" && typeof (error as { message?: unknown }).message === "string"
        ? (error as { message: string }).message : "native port refused the operation";
      pending.reject(new RuntimeRefused(message));
    } else pending.resolve(envelope.result);
  };

  constructor(bridge: NativeMessageBridge) {
    this.#bridge = bridge;
    bridge.addEventListener("message", this.#receive);
  }

  call(payload: Record<string, unknown>, signal?: AbortSignal): Promise<unknown> {
    if (this.#closed) return Promise.reject(new RuntimeRefused("native message channel is closed"));
    const cleanup = ["ownership.revoke", "ownership.drainAndRelease", "sqlite.rollback", "sqlite.close"].includes(String(payload.method));
    if (this.#pending.size >= 1024 && !cleanup) return Promise.reject(new RuntimeRefused("native channel request limit reached"));
    signal?.throwIfAborted();
    const id = `js:${this.#prefix}:${++this.#sequence}`;
    return new Promise((resolve, reject) => {
      const cancel = () => {
        // Do not lose an ownership/SQLite resource returned after a race.
        // The promise settles only with the native terminal response; the
        // caller rechecks its generation/abort signal before publishing it.
        try {
          this.#bridge.postMessage(JSON.stringify({ channel: NATIVE_CHANNEL, payload: {
            method: "request.cancel", requestId: id,
            ownerHandle: payload.ownerHandle, generation: payload.generation,
          } }));
        } catch { /* Native teardown owns revoke/drain if the channel broke. */ }
      };
      this.#pending.set(id, { resolve, reject, detach: () => signal?.removeEventListener("abort", cancel) });
      signal?.addEventListener("abort", cancel, { once: true });
      try { this.#bridge.postMessage(JSON.stringify({ channel: NATIVE_CHANNEL, id, payload })); }
      catch (error) {
        this.#pending.delete(id);
        signal?.removeEventListener("abort", cancel);
        reject(error);
      }
    });
  }

  bindOwner(lease: OwnerLease): void {
    if (this.#closed || this.#owner) throw new RuntimeRefused("the native channel already has an owner");
    this.#owner = Object.freeze({ ...lease });
  }

  #savedFailure(message: string): void {
    if (this.#closed) return;
    try {
      this.#bridge.postMessage(JSON.stringify({ channel: NATIVE_CHANNEL, event: "operation.failed", message }));
    } catch {
      // A broken platform channel owns native revoke and teardown. Reject
      // local waiters without creating an unhandled rejection or retry.
      this.close();
    }
  }

  onRevoked(lease: OwnerLease, listener: () => void): () => void {
    const matching = (revoked: Pick<OwnerLease, "handle" | "generation">) => {
      if (revoked.handle === lease.handle && revoked.generation === lease.generation) listener();
    };
    this.#revocations.add(matching);
    if (this.#revoked.has(JSON.stringify([lease.handle, lease.generation]))) listener();
    return () => { this.#revocations.delete(matching); };
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#bridge.removeEventListener("message", this.#receive);
    for (const pending of this.#pending.values()) {
      pending.detach(); pending.reject(new RuntimeRefused("native message channel was closed"));
    }
    this.#pending.clear();
    this.#revocations.clear();
    this.#revoked.clear();
    this.#savedAcks.clear();
    this.#owner = undefined;
  }
}

/** All privilege-bearing frames are bound to the native-issued lease. */
export function messageNativePorts(wire: NativeWireClient): NativePorts {
  const bound = (lease: OwnerLease, method: string, fields: Record<string, unknown> = {}, signal?: AbortSignal) =>
    wire.call({ ...fields, method, ownerHandle: lease.handle, generation: lease.generation }, signal);
  return {
    ownership: {
      async acquire(storageId) {
        const lease = record(await wire.call({ method: "ownership.acquire", storageId }));
        if (typeof lease.storageId !== "string" || typeof lease.handle !== "string" || typeof lease.generation !== "string") {
          throw new RuntimeRefused("native ownership response is incomplete");
        }
        const owner = { storageId: lease.storageId, handle: lease.handle, generation: lease.generation };
        wire.bindOwner(owner);
        return owner;
      },
      async confirm(lease, snapshot, approval) {
        const grant = record(await bound(lease, "ownership.confirm", { snapshot, approval }));
        if (typeof grant.generation !== "string" || typeof grant.snapshotId !== "string"
          || typeof grant.allowInitializeEmptyStore !== "boolean" || (grant.networkPermit !== undefined && typeof grant.networkPermit !== "string")) {
          throw new RuntimeRefused("native execution grant is incomplete");
        }
        return grant as unknown as ExecutionGrant;
      },
      async revoke(lease) { await bound(lease, "ownership.revoke"); },
      async drainAndRelease(lease) { await bound(lease, "ownership.drainAndRelease"); },
      onRevoked: (lease, listener) => wire.onRevoked(lease, listener),
    },
    recovery: {
      async read(lease, signal) { return record(await bound(lease, "recovery.read", {}, signal)) as unknown as RecoverySnapshot; },
    },
    sqlite: nativeSqlitePort({ send: (frame, signal) => wire.call({ ...frame }, signal) }),
    mcp: {
      async send(lease, message, signal, invocation) {
        const result = await bound(lease, "mcp.send", { message, ...(invocation ? { invocation } : {}) }, signal);
        // JSON has no undefined; a native notification has a null reply.
        return result === null ? undefined : result;
      },
    },
    network: {
      async fetch() {
        // This first host profile has no native stream transport or model
        // service configuration. No browser fetch fallback is permitted.
        throw new RuntimeRefused("native model streaming transport and cost authorization are not configured");
      },
    },
  };
}

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new RuntimeRefused("native message must contain an object");
  return value as Record<string, unknown>;
}
