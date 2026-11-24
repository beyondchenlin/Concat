// SPDX-License-Identifier: AGPL-3.0-or-later

import type { Context } from "@earendil-works/chord/context";
import type { Cursor, Harness, HarnessInspection, Storage, SubmissionRecord } from "@earendil-works/pi-durable";
import { ExecutionGate, RuntimeRefused } from "./gate.ts";
import type { OwnerLease } from "./ports.ts";

const TASKS: Readonly<Record<string, { version: number; phases: readonly string[] }>> = {
  "pi.generation": { version: 3, phases: ["prepare", "request", "retry", "tools", "finish", "drain", "afterTools", "yield"] },
  "pi.tool": { version: 4, phases: ["call", "execute", "parked"] },
  "pi.compaction": { version: 1, phases: ["select", "summarize", "retry"] },
};
const LIVE_TASK_STATES = new Set(["pending", "running", "waiting", "sleeping", "completing"]);

type InspectedTask = HarnessInspection["tasks"][number]["record"];

export function reviewTask(record: InspectedTask, allowed: ReadonlySet<number>): void {
  if (!LIVE_TASK_STATES.has(record.state.status)) throw new RuntimeRefused("agent.errorRecoverySnapshotInvalid");
  if (!allowed.has(record.conversationId)) throw new RuntimeRefused("agent.errorRecoveryOtherProject");
  const known = Object.hasOwn(TASKS, record.kind) ? TASKS[record.kind] : undefined;
  if (!known || record.version !== known.version) throw new RuntimeRefused("agent.errorRecoverySnapshotInvalid");
  if (record.state.status !== "completing" && record.state.status !== "terminal") {
    const checkpoint = record.state.checkpoint;
    if (!checkpoint || typeof checkpoint !== "object" || Array.isArray(checkpoint)
      || typeof checkpoint.phase !== "string" || !known.phases.includes(checkpoint.phase)
      || (record.kind === "pi.generation" && "poll" in checkpoint)) {
      throw new RuntimeRefused("agent.errorRecoveryScope");
    }
  }
}

function reviewSubmission(submission: SubmissionRecord, allowed: ReadonlySet<number>): void {
  if (!allowed.has(submission.conversationId)) throw new RuntimeRefused("agent.errorRecoveryOtherProject");
  if (!["input", "write", "continue"].includes(submission.type) || !["queued", "placed"].includes(submission.status)) {
    throw new RuntimeRefused("agent.errorRecoverySnapshotInvalid");
  }
}

function assertLocal(gate: ExecutionGate, lease: OwnerLease): void {
  gate.assertStorageWrite(lease);
  if (gate.phase !== "activating") throw new RuntimeRefused("agent.errorRecoveryChanged");
}

function fixedFailure(error: unknown): never {
  if (error instanceof RuntimeRefused) throw error;
  // Never forward raw stored/provider error or model text. Original records
  // retain their actual outcomes for a separately reviewed diagnostic path.
  throw new RuntimeRefused("agent.errorRecoveryChanged");
}

/** Harness.open writes running -> pending even with scheduling paused. Use
 * only original storage reads to reject unrelated/unknown records before
 * permitting that initial reconciliation to touch the shared database. */
export async function reviewDiscardStorage(
  storage: Storage, conversationIds: readonly number[], gate: ExecutionGate, lease: OwnerLease, context: Context,
  mustBeSettled = false,
): Promise<void> {
  try {
    const allowed = new Set(conversationIds);
    assertLocal(gate, lease);
    // Never filter by a known state: a future/corrupt state would disappear
    // from that query while native recovery still correctly counts it live.
    for (const kind of ["tasks", "submissions"] as const) {
      let cursor: Cursor | undefined;
      let previousId = -1;
      do {
        const page = kind === "tasks"
          ? await storage.scanTasks({}, 256, cursor, context)
          : await storage.scanSubmissions({}, 256, cursor, context);
        assertLocal(gate, lease);
        for (const record of page.items) {
          if (!Number.isSafeInteger(record.id) || record.id <= previousId) {
            throw new RuntimeRefused("agent.errorRecoverySnapshotInvalid");
          }
          previousId = record.id;
          if (kind === "tasks") {
            const task = record as InspectedTask;
            if (task.state.status === "terminal") continue;
            reviewTask(task, allowed);
          } else {
            const submission = record as SubmissionRecord;
            if (["done", "unanswered"].includes(submission.status)) continue;
            reviewSubmission(submission, allowed);
          }
          if (mustBeSettled) throw new RuntimeRefused("agent.errorRecoveryChanged");
        }
        cursor = page.next;
      } while (cursor !== undefined);
    }
  } catch (error) { fixedFailure(error); }
}

/** Validate the entire shared store before writing even one cancellation.
 * These versions and abort handlers were read at the pinned original commit.
 * A generation's deferred poll can call a provider even while aborting it;
 * it therefore cannot use this strictly local discard path. */
function review(inspection: HarnessInspection, allowed: ReadonlySet<number>, marked: boolean): void {
  if (inspection.scheduling !== "paused") throw new RuntimeRefused("agent.errorRecoveryChanged");
  for (const { record, state } of inspection.tasks) {
    reviewTask(record, allowed);
    if (state.kind === "blocked" || state.kind === "running"
      || (state.kind === "pending" && state.migrates)) {
      throw new RuntimeRefused("agent.errorRecoveryScope");
    }
    if (marked && record.abortRequested !== true) throw new RuntimeRefused("agent.errorRecoveryChanged");
  }
  for (const submission of inspection.submissions) {
    reviewSubmission(submission, allowed);
    if (marked && submission.status === "queued") {
      throw new RuntimeRefused("agent.errorRecoveryChanged");
    }
  }
}

/** A reviewed cancellation barrier over the original scheduler, not a model
 * or task loop. No native task is active, and the gate stays activating until
 * the final empty inspection. The only installed tools have no cancel hook. */
export async function discardOriginalWork(
  harness: Harness, conversationIds: readonly number[], gate: ExecutionGate, lease: OwnerLease, context: Context,
): Promise<void> {
  const assert = () => assertLocal(gate, lease);
  try {
    assert();
    const before = await harness.inspect(context);
    assert();
    const allowed = new Set(conversationIds);
    review(before, allowed, false);
    // abortSubmission and abortTask do not resume the pinned scheduler.
    // A queued write/continue is cancelled too; nothing can later dispatch it.
    for (const submission of before.submissions) {
      if (submission.status !== "queued") continue;
      const outcome = await harness.abortSubmission(submission.id, context, submission.conversationId);
      assert();
      if (outcome !== "aborted" && outcome !== "settled") throw new RuntimeRefused("agent.errorRecoveryChanged");
    }
    for (const { record } of before.tasks) {
      await harness.abortTask(record.id, context);
      assert();
    }
    const cancelled = await harness.inspect(context);
    assert();
    review(cancelled, allowed, true);
    // Only after the complete barrier may these original methods resume.
    // Wait for every task, including background work excluded by waitForIdle.
    for (const { record } of before.tasks) {
      const settled = await harness.waitForTask(record.id, context);
      assert();
      if (settled.state.outcome.status !== "aborted" && record.state.status !== "completing") {
        throw new RuntimeRefused("agent.errorRecoveryChanged");
      }
    }
    await harness.waitForIdle(context);
    assert();
    const after = await harness.inspect(context);
    assert();
    if (after.tasks.length || after.submissions.length) throw new RuntimeRefused("agent.errorRecoveryChanged");
  } catch (error) { fixedFailure(error); }
}
