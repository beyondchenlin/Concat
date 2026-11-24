// SPDX-License-Identifier: AGPL-3.0-or-later
//! Deliberate recovery over the pinned original scheduler. This file never
//! rewrites a checkpoint, submits a replacement input or replays native edits.

import type { Context } from "@earendil-works/chord/context";
import type { Cursor, HarnessInspection, Storage, SubmissionRecord } from "@earendil-works/pi-durable";
import { RuntimeRefused, type ExecutionGate } from "./gate.ts";
import type { OwnerLease } from "./ports.ts";
import type { NativeRecoveryScope } from "./commands.ts";
import { reviewTask } from "./recovery-discard.ts";

type TaskRecord = HarnessInspection["tasks"][number]["record"];
const terminal = (record: SubmissionRecord) => record.status === "done" || record.status === "unanswered";

function checkModel(record: TaskRecord, scope: NativeRecoveryScope): void {
  if (record.abortRequested || record.background) throw new RuntimeRefused("agent.errorRecoveryScope");
  if (record.state.status === "terminal" || record.state.status === "completing") return;
  const checkpoint = record.state.checkpoint;
  if (!checkpoint || typeof checkpoint !== "object" || Array.isArray(checkpoint)) throw new RuntimeRefused("agent.errorRecoveryScope");
  if (checkpoint.model !== undefined) {
    const model = checkpoint.model;
    if (!model || typeof model !== "object" || Array.isArray(model)
      || model.provider !== scope.modelConfiguration.id || model.modelId !== scope.modelConfiguration.model) {
      throw new RuntimeRefused("agent.errorRecoveryConnectionChanged");
    }
  }
}

function checkSubmission(record: SubmissionRecord, scope: NativeRecoveryScope): void {
  if (record.id !== scope.submissionId || record.conversationId !== scope.conversationId
    || record.requestId !== scope.taskId || record.type !== "input"
    || !["queued", "placed", "done", "unanswered"].includes(record.status)) {
    throw new RuntimeRefused("agent.errorRecoveryScope");
  }
}

/** Review every live record in the shared original store before Harness.open
 * can reconcile running tasks to pending. No history-size execution ceiling. */
export async function reviewResumeStorage(
  storage: Storage, scope: NativeRecoveryScope, gate: ExecutionGate, lease: OwnerLease, context: Context,
): Promise<void> {
  const assert = () => {
    gate.assertStorageWrite(lease);
    if (gate.phase !== "activating") throw new RuntimeRefused("agent.errorRecoveryChanged");
  };
  const allowed = new Set([scope.conversationId]);
  const expected = new Set(scope.taskIds);
  const found = new Set<number>();
  let foundSubmission = false;
  for (const kind of ["tasks", "submissions"] as const) {
    let cursor: Cursor | undefined;
    let previous = -1;
    do {
      assert();
      const page = kind === "tasks"
        ? await storage.scanTasks({}, 256, cursor, context)
        : await storage.scanSubmissions({}, 256, cursor, context);
      assert();
      for (const record of page.items) {
        if (!Number.isSafeInteger(record.id) || record.id <= previous) throw new RuntimeRefused("agent.errorRecoveryScope");
        previous = record.id;
        if (kind === "tasks") {
          const task = record as TaskRecord;
          if (task.state.status === "terminal") continue;
          reviewTask(task, allowed);
          checkModel(task, scope);
          if (!expected.has(task.id)) throw new RuntimeRefused("agent.errorRecoveryScope");
          found.add(task.id);
        } else {
          const submission = record as SubmissionRecord;
          if (submission.id === scope.submissionId) {
            checkSubmission(submission, scope);
            foundSubmission = true;
          } else if (!terminal(submission)) {
            throw new RuntimeRefused("agent.errorRecoveryScope");
          }
        }
      }
      cursor = page.next;
    } while (cursor !== undefined);
  }
  if (!foundSubmission || found.size !== expected.size) throw new RuntimeRefused("agent.errorRecoveryScope");
}

/** Harness.open must still be paused. Unknown definitions, migrations, foreign
 * tasks and abort marks are not granted authority by a successful file read. */
export function reviewResumeInspection(inspection: HarnessInspection, scope: NativeRecoveryScope): void {
  if (inspection.scheduling !== "paused") throw new RuntimeRefused("agent.errorRecoveryChanged");
  const expected = new Set(scope.taskIds);
  const found = new Set<number>();
  for (const { record, state } of inspection.tasks) {
    reviewTask(record, new Set([scope.conversationId]));
    checkModel(record, scope);
    if (!expected.has(record.id) || state.kind === "blocked" || state.kind === "running"
      || (state.kind === "pending" && state.migrates)) throw new RuntimeRefused("agent.errorRecoveryScope");
    found.add(record.id);
  }
  if (found.size !== expected.size) throw new RuntimeRefused("agent.errorRecoveryScope");
  for (const submission of inspection.submissions) checkSubmission(submission, scope);
}
