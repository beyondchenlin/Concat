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
const MAX_SCAN_RECORDS = 1_000_000;

type InspectedTask = HarnessInspection["tasks"][number]["record"];

function reviewTask(record: InspectedTask, allowed: ReadonlySet<number>): void {
  if (!LIVE_TASK_STATES.has(record.state.status)) throw new RuntimeRefused("存在未知历史任务状态，未自动放弃。");
  if (!allowed.has(record.conversationId)) throw new RuntimeRefused("仍有其他工程或未绑定会话的任务，请切换到对应工程处理。");
  const known = Object.hasOwn(TASKS, record.kind) ? TASKS[record.kind] : undefined;
  if (!known || record.version !== known.version) throw new RuntimeRefused("存在未经审核的任务种类或版本，未自动放弃。");
  if (record.state.status !== "completing" && record.state.status !== "terminal") {
    const checkpoint = record.state.checkpoint;
    if (!checkpoint || typeof checkpoint !== "object" || Array.isArray(checkpoint)
      || typeof checkpoint.phase !== "string" || !known.phases.includes(checkpoint.phase)
      || (record.kind === "pi.generation" && "poll" in checkpoint)) {
      throw new RuntimeRefused("存在延期网络轮询或未经审核的任务阶段，未自动放弃。");
    }
  }
}

function reviewSubmission(submission: SubmissionRecord, allowed: ReadonlySet<number>): void {
  if (!allowed.has(submission.conversationId)) throw new RuntimeRefused("仍有其他工程或未绑定会话的输入，请切换到对应工程处理。");
  if (!["input", "write", "continue"].includes(submission.type) || !["queued", "placed"].includes(submission.status)) {
    throw new RuntimeRefused("存在未经审核的历史输入状态，未自动放弃。");
  }
}

function assertLocal(gate: ExecutionGate, lease: OwnerLease): void {
  gate.assertStorageWrite(lease);
  if (gate.phase !== "activating") throw new RuntimeRefused("放弃操作只能在原生恢复核对阶段执行。");
}

function fixedFailure(error: unknown): never {
  if (error instanceof RuntimeRefused) throw error;
  // Never forward raw stored/provider error or model text. Original records
  // retain their actual outcomes for a separately reviewed diagnostic path.
  throw new RuntimeRefused("原版会话取消或持久化未能完成，已有记录保留，运行组件未进入就绪状态。");
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
      let reviewed = 0;
      do {
        const page = kind === "tasks"
          ? await storage.scanTasks({}, 256, cursor, context)
          : await storage.scanSubmissions({}, 256, cursor, context);
        assertLocal(gate, lease);
        for (const record of page.items) {
          if (++reviewed > MAX_SCAN_RECORDS || !Number.isSafeInteger(record.id) || record.id <= previousId) {
            throw new RuntimeRefused("历史记录超出审核上限或分页编号不一致，未自动放弃。");
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
          if (mustBeSettled) throw new RuntimeRefused("原版存储仍有未完成记录，会话未恢复就绪。");
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
  if (inspection.scheduling !== "paused") throw new RuntimeRefused("会话执行器未暂停，不能安全放弃历史任务。");
  for (const { record, state } of inspection.tasks) {
    reviewTask(record, allowed);
    if (state.kind === "blocked" || state.kind === "running"
      || (state.kind === "pending" && state.migrates)) {
      throw new RuntimeRefused("存在未经审核的任务种类、版本或迁移状态，未自动放弃。");
    }
    if (marked && record.abortRequested !== true) throw new RuntimeRefused("历史任务未全部标记取消，会话未恢复就绪。");
  }
  for (const submission of inspection.submissions) {
    reviewSubmission(submission, allowed);
    if (marked && submission.status === "queued") {
      throw new RuntimeRefused("历史输入没有全部安全撤销，会话未恢复就绪。");
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
      if (outcome !== "aborted" && outcome !== "settled") throw new RuntimeRefused("历史排队输入未能撤销，会话未恢复就绪。");
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
        throw new RuntimeRefused("原版任务取消没有正常结束，未将会话标记为就绪。");
      }
    }
    await harness.waitForIdle(context);
    assert();
    const after = await harness.inspect(context);
    assert();
    if (after.tasks.length || after.submissions.length) throw new RuntimeRefused("仍有未完成会话记录，未自动恢复新任务。");
  } catch (error) { fixedFailure(error); }
}
