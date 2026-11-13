// SPDX-License-Identifier: AGPL-3.0-or-later

import type { SqliteDatabase } from "@earendil-works/chord/sqlite";

/** Opaque native authority, not an authority constructed by JavaScript. */
export interface OwnerLease {
  readonly storageId: string;
  readonly generation: string;
  readonly handle: string;
}

/** Native read-only reconciliation over the current project and SQLite store. */
export interface RecoverySnapshot {
  readonly snapshotId: string;
  readonly projectSession: string;
  readonly projectRevision: string;
  readonly sessionRevision: string;
  readonly schemaVersion: number | "empty";
  readonly hasInterruptedWork: boolean;
}

export interface RecoveryApproval {
  readonly snapshotId: string;
  readonly decision: "continue" | "discard";
  /** Issued by the native user action, never by a model tool. */
  readonly approvalId: string;
}

export interface ExecutionGrant {
  readonly generation: string;
  readonly snapshotId: string;
  readonly allowInitializeEmptyStore: boolean;
  /** Absent until a model channel and its cost authorization exist. */
  readonly networkPermit?: string;
}

/** Original Pi ToolExecutionApi identity, never read from model arguments. */
export interface InvocationContext {
  readonly conversationId: number;
  readonly taskId: number;
  readonly callId: string;
}

/**
 * Every native privileged operation must validate the same lease inside
 * its actual write/dispatch boundary, not merely when returning this port.
 * The JS gate complements the native OS lock and generation; it replaces
 * neither. Ownership is acquired before any writable SQLite open.
 */
export interface NativePorts {
  readonly ownership: {
    acquire(storageId: string): Promise<OwnerLease>;
    confirm(lease: OwnerLease, snapshot: RecoverySnapshot, approval: RecoveryApproval): Promise<ExecutionGrant>;
    revoke(lease: OwnerLease): Promise<void>;
    drainAndRelease(lease: OwnerLease): Promise<void>;
    onRevoked(lease: OwnerLease, listener: () => void): () => void;
  };
  readonly recovery: {
    /** SELECT-only. Must not initialize/migrate SQLite or resume Pi. */
    read(lease: OwnerLease, signal: AbortSignal): Promise<RecoverySnapshot>;
  };
  readonly sqlite: {
    /**
     * Return the portable Pi SQLite facade over this native session store.
     * Reject any schema other than expectedSchemaVersion or an explicitly
     * approved empty store. Never migrate an existing older database.
     * All transaction statements and COMMIT retain and recheck the lease.
     */
    open(lease: OwnerLease, options: {
      expectedSchemaVersion: number;
      allowInitializeEmptyStore: boolean;
      signal: AbortSignal;
    }): Promise<SqliteDatabase>;
  };
  readonly mcp: {
    /** One frame to the native concat_mcp::Session, with no retry/fallback. */
    send(lease: OwnerLease, message: unknown, signal: AbortSignal, invocation?: InvocationContext): Promise<unknown | undefined>;
  };
  readonly network: {
    /** Native code validates ownership, channel, destination and budget. */
    fetch(lease: OwnerLease, networkPermit: string, input: RequestInfo | URL, init: RequestInit): Promise<Response>;
  };
}

export type RuntimePhase = "new" | "acquiring" | "recovery" | "activating" | "ready" | "stopping" | "closed";

/** Exhaustive host actions that can enable Pi's original scheduler. */
export type SchedulingAction =
  | "createConversation" | "submit" | "waitSubmission" | "abortSubmission"
  | "resume" | "abortTask" | "signal" | "waitForTask" | "waitForIdle"
  | "compact" | "abortConversation" | "resetConversation";

export interface PiEngine {
  inspect(): Promise<unknown>;
  observeConversation(conversationId: number, listener: (event: ConversationEvent) => void): Promise<ConversationObservation>;
  run(action: SchedulingAction, payload: unknown): Promise<unknown>;
  close(): Promise<void>;
}

/** Original Pi view data, with no synthetic assistant messages. */
export type ConversationEvent =
  | { readonly kind: "value"; readonly value: unknown }
  | { readonly kind: "failed"; readonly message: string }
  | { readonly kind: "ended" };

export interface ConversationObservation {
  readonly value: unknown;
  detach(): void;
}

export interface EngineOpenOptions {
  readonly lease: OwnerLease;
  readonly grant: ExecutionGrant;
  readonly ports: NativePorts;
  readonly gate: import("./gate.ts").ExecutionGate;
}

export interface PiEngineFactory {
  open(options: EngineOpenOptions): Promise<PiEngine>;
}
