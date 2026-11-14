// SPDX-License-Identifier: AGPL-3.0-or-later

import { ExecutionGate, RuntimeRefused } from "./gate.ts";
import type {
  NativePorts, OwnerLease, PiEngine, PiEngineFactory, RecoveryApproval,
  RecoverySnapshot, SchedulingAction, ConversationEvent, ConversationObservation,
} from "./ports.ts";

const ACTIONS = new Set<SchedulingAction>([
  "createConversation", "submit", "waitSubmission", "abortSubmission", "resume", "abortTask",
  "signal", "waitForTask", "waitForIdle", "compact", "abortConversation", "resetConversation", "nativeSubmit", "nativeWait",
]);

/**
 * One native-authorized Pi host lifecycle. No task queue, retry loop, model
 * loop or durable state implementation lives here. A replacement instance
 * starts in recovery and never inherits an execution grant.
 */
export class AgentRuntime {
  readonly #storageId: string;
  readonly #ports: NativePorts;
  readonly #factory: PiEngineFactory;
  readonly #gate = new ExecutionGate();
  #lease: OwnerLease | undefined;
  #engine: PiEngine | undefined;
  #snapshot: RecoverySnapshot | undefined;
  #transition: Promise<unknown> = Promise.resolve();
  #stopping: Promise<void> | undefined;
  #unsubscribe: (() => void) | undefined;
  #confirming = false;
  #nativeConfirmed = false;
  readonly #observations = new Set<() => void>();

  constructor(storageId: string, ports: NativePorts, factory: PiEngineFactory) {
    if (!storageId) throw new RuntimeRefused("a native session store is required");
    this.#storageId = storageId;
    this.#ports = ports;
    this.#factory = factory;
  }

  get phase() { return this.#gate.phase; }

  start(): Promise<RecoverySnapshot> {
    this.#gate.acquiring();
    const work = this.#start();
    this.#transition = work;
    return work.catch(async (error: unknown) => {
      await this.stop().catch(() => {});
      throw error;
    });
  }

  async #start(): Promise<RecoverySnapshot> {
    const lease = Object.freeze({ ...await this.#ports.ownership.acquire(this.#storageId) });
    // Keep the lease even if stop won the race, so stop can revoke and
    // release it after this transition settles.
    this.#lease = lease;
    if (lease.storageId !== this.#storageId) throw new RuntimeRefused("native ownership returned another store");
    this.#gate.owned(lease);
    this.#unsubscribe = this.#ports.ownership.onRevoked(lease, () => {
      void this.stop().catch(() => { /* Native retains the lock on a failed drain. */ });
    });
    const snapshot = await this.#ports.recovery.read(lease, this.#gate.signal);
    this.#gate.assertOwned(lease);
    this.#snapshot = validateSnapshot(snapshot);
    return { ...this.#snapshot };
  }

  async inspect(): Promise<unknown> {
    const lease = this.#requireLease();
    this.#gate.assertOwned(lease);
    if (this.#engine) {
      const result = await this.#engine.inspect();
      this.#gate.assertOwned(lease);
      return result;
    }
    const snapshot = await this.#ports.recovery.read(lease, this.#gate.signal);
    this.#gate.assertOwned(lease);
    this.#snapshot = validateSnapshot(snapshot);
    return { ...this.#snapshot };
  }

  confirmRecovery(approval: RecoveryApproval): Promise<void> {
    if (this.phase !== "recovery" || this.#confirming) {
      return Promise.reject(new RuntimeRefused("recovery is not awaiting a decision"));
    }
    this.#confirming = true;
    const work = this.#activate(approval).finally(() => { this.#confirming = false; });
    this.#transition = work;
    return work.catch(async (error: unknown) => {
      if (this.phase === "activating" || this.#nativeConfirmed) await this.stop().catch(() => {});
      throw error;
    });
  }

  async #activate(approval: RecoveryApproval): Promise<void> {
    const lease = this.#requireLease();
    this.#gate.assertOwned(lease);
    const snapshot = this.#snapshot;
    if (!snapshot || approval.snapshotId !== snapshot.snapshotId || !approval.approvalId
      || !["continue", "discard"].includes(approval.decision)) {
      throw new RuntimeRefused("the native approval must match the last reconciliation snapshot");
    }
    const issued = await this.#ports.ownership.confirm(lease, snapshot, approval);
    this.#nativeConfirmed = true;
    this.#gate.assertOwned(lease);
    if (issued.snapshotId !== snapshot.snapshotId) throw new RuntimeRefused("native grant has a stale snapshot");
    const ids = issued.discardConversationIds;
    if (ids !== undefined && (!Array.isArray(ids) || ids.length === 0 || ids.length > 65536
      || new Set(ids).size !== ids.length || Array.from(ids).some((id) => !Number.isSafeInteger(id) || id < 0)
      || approval.decision !== "discard" || !snapshot.hasInterruptedWork || snapshot.schemaVersion === "empty"
      || issued.allowInitializeEmptyStore || issued.networkPermit !== undefined)) {
      throw new RuntimeRefused("原生放弃授权不完整或同时包含调用许可，未打开会话存储。");
    }
    if (snapshot.hasInterruptedWork && (approval.decision !== "discard" || ids === undefined)) {
      throw new RuntimeRefused("未完成任务需要当前工程的原生明确放弃授权，未自动恢复或清除。");
    }
    const grant = { ...issued, ...(ids === undefined ? {} : { discardConversationIds: Object.freeze([...ids]) }) };
    this.#gate.activating(lease, grant);
    const engine = await this.#factory.open({ lease, grant, ports: this.#ports, gate: this.#gate });
    this.#engine = engine;
    if (ids !== undefined) {
      this.#gate.assertStorageWrite(lease);
      await this.#ports.recovery.discardVerified(lease, this.#gate.signal);
      this.#gate.assertStorageWrite(lease);
    }
    this.#gate.ready(lease);
    // Ordinary open never resumes. A discard grant is consumed inside the
    // factory while still activating, before tools/network can be admitted.
  }

  async dispatch(action: SchedulingAction, payload?: unknown): Promise<unknown> {
    if (!ACTIONS.has(action)) throw new RuntimeRefused("unreviewed Pi scheduling entry point");
    const lease = this.#requireLease();
    this.#gate.assertReady(lease);
    const engine = this.#engine;
    if (!engine) throw new RuntimeRefused("Pi is not open");
    const result = await engine.run(action, payload);
    this.#gate.assertReady(lease);
    return result;
  }

  async observeConversation(conversationId: number, listener: (event: ConversationEvent) => void): Promise<ConversationObservation> {
    const lease = this.#requireLease();
    this.#gate.assertReady(lease);
    if (!this.#engine) throw new RuntimeRefused("Pi is not open");
    const observation = await this.#engine.observeConversation(conversationId, (event) => {
      try { this.#gate.assertReady(lease); } catch { return; }
      listener(event);
    });
    try { this.#gate.assertReady(lease); } catch (error) { observation.detach(); throw error; }
    const detach = () => { try { observation.detach(); } finally { this.#observations.delete(detach); } };
    this.#observations.add(detach);
    return { value: observation.value, detach };
  }

  /** Revoke before Pi cleanup, drain before the next owner can acquire. */
  stop(): Promise<void> {
    if (this.#stopping) return this.#stopping;
    this.#gate.stop();
    const detachErrors: unknown[] = [];
    for (const detach of this.#observations) {
      try { detach(); } catch (error) { detachErrors.push(error); }
    }
    const transition = this.#transition;
    // Store the promise before calling a native method: revoke may
    // synchronously deliver onRevoked, which calls stop again.
    let resolve!: () => void;
    let reject!: (error: unknown) => void;
    this.#stopping = new Promise<void>((done, fail) => { resolve = done; reject = fail; });
    void this.#stop(transition, detachErrors).then(resolve, reject);
    return this.#stopping;
  }

  async #stop(transition: Promise<unknown>, errors: unknown[]): Promise<void> {
    let revoked: OwnerLease | undefined;
    const revoke = async () => {
      if (!this.#lease || revoked === this.#lease) return;
      await this.#ports.ownership.revoke(this.#lease);
      revoked = this.#lease;
    };
    try { await revoke(); } catch (error) { errors.push(error); }
    // An acquire or engine open that was already underway may return
    // after stop. It remains part of this host until it can be drained.
    await transition.catch(() => {});
    try { await revoke(); } catch (error) { errors.push(error); }
    try { this.#unsubscribe?.(); } catch (error) { errors.push(error); }
    this.#unsubscribe = undefined;
    try { await this.#engine?.close(); } catch (error) { errors.push(error); }
    this.#engine = undefined;
    if (this.#lease) {
      // This native operation must retain OS ownership if drain fails.
      await this.#ports.ownership.drainAndRelease(this.#lease);
    }
    this.#gate.closed();
    if (errors.length) throw new AggregateError(errors, "runtime closed with cleanup failures");
  }

  #requireLease(): OwnerLease {
    if (!this.#lease) throw new RuntimeRefused("native ownership has not been acquired");
    return this.#lease;
  }
}

function validateSnapshot(snapshot: RecoverySnapshot): RecoverySnapshot {
  if (!snapshot.snapshotId || !snapshot.projectSession || !snapshot.projectRevision || !snapshot.sessionRevision
    || typeof snapshot.hasInterruptedWork !== "boolean"
    || !(snapshot.schemaVersion === "empty" || (Number.isSafeInteger(snapshot.schemaVersion) && snapshot.schemaVersion >= 0))) {
    throw new RuntimeRefused("native recovery snapshot is incomplete");
  }
  return Object.freeze({ ...snapshot });
}
