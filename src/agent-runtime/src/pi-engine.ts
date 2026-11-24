// SPDX-License-Identifier: AGPL-3.0-or-later

import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import {
  Harness, createRegistry, type ConversationId, type SubmissionId, type TaskId,
  type ToolRegistration, type AgentChange, type Storage,
} from "@earendil-works/pi-durable";
import { CURRENT_SQLITE_SCHEMA_VERSION, SqliteStorage } from "@earendil-works/pi-durable/storage/sqlite";
// The pinned package root also re-exports its Node stdio transport. Use
// these reviewed original source modules, without copying a client or
// substituting browser stubs for Node's process/filesystem modules.
import { McpClient } from "@bk-cat/pi-reference/mcp/client";
import { normalizeMcpInputSchema } from "@bk-cat/pi-reference/mcp/input-schema";
import { toLlmContent } from "@bk-cat/pi-reference/mcp/content";
import { RuntimeRefused } from "./gate.ts";
import { taskLanguages, languageInstructions } from "./languages.ts";
import { argumentObject, conversationOptions, submissionDraft, nativeTaskInput, type NativeTaskInput } from "./commands.ts";
import { nativeModels, type ModelConfiguration } from "./models.ts";
import { nativeModelConfiguration } from "./model-configuration.ts";
import { EMBEDDED_TOOLS, guardedDatabase, guardedStorage, NativeMcpTransport } from "./native-adapters.ts";
import type { EngineOpenOptions, PiEngine, PiEngineFactory, SchedulingAction } from "./ports.ts";
import { discardOriginalWork, reviewDiscardStorage } from "./recovery-discard.ts";
import { nativeRecoveryScope, nativeResumeInput, assertRecoveryIdentity } from "./commands.ts";
import { reviewResumeStorage, reviewResumeInspection } from "./recovery-resume.ts";

/** Builds only the original pinned Pi Harness and portable SqliteStorage. */
export function piEngineFactory(configuration: ModelConfiguration): PiEngineFactory {
  return {
    async open(options: EngineOpenOptions): Promise<PiEngine> {
      const { ports, gate, lease, grant } = options;
      const recovery = grant.resumeTask === undefined ? undefined : nativeRecoveryScope(grant.resumeTask);
      gate.assertStorageWrite(lease);
      const database = await ports.sqlite.open(lease, {
        expectedSchemaVersion: CURRENT_SQLITE_SCHEMA_VERSION,
        allowInitializeEmptyStore: grant.allowInitializeEmptyStore,
        signal: gate.signal,
      });
      const client = new McpClient({ name: "bk-cat-native", version: "0.1.0", protocolVersion: "2025-11-25" });
      const transport = new NativeMcpTransport(ports, lease, gate);
      let storage: SqliteStorage | undefined;
      let harness: Harness | undefined;
      const nativeTask: NativeTaskState = {};
      const taskConfiguration = { current: recovery === undefined ? configuration : nativeModelConfiguration(recovery.modelConfiguration) };
      try {
        gate.assertStorageWrite(lease);
        storage = await SqliteStorage.open(guardedDatabase(database, gate, lease));
        gate.assertStorageWrite(lease);
        await client.connect(transport);
        gate.assertStorageWrite(lease);
        const offered = await client.listTools({ signal: gate.signal });
        if (offered.length !== EMBEDDED_TOOLS.length
          || new Set(offered.map((tool) => tool.name)).size !== EMBEDDED_TOOLS.length
          || offered.some((tool) => !(EMBEDDED_TOOLS as readonly string[]).includes(tool.name))) {
          throw new RuntimeRefused("native MCP tool surface differs from the reviewed embedded profile");
        }
        const registry = createRegistry();
        const tools: ToolRegistration[] = offered.map((tool) => ({
          name: tool.name,
          description: tool.description ?? tool.name,
          parameters: normalizeMcpInputSchema(tool.inputSchema),
          replay: "unsafe",
          executionMode: "sequential",
          async execute(args, api, context) {
            gate.assertReady(lease);
            if (!nativeTask.active || nativeTask.active.conversationId !== api.conversationId
              || (tool.name === "edit_apply_guarded" && nativeTask.active.mode !== "edit")) {
              throw new RuntimeRefused("this native task does not authorize the requested tool");
            }
            const argumentsObject = { ...args as Record<string, unknown> };
            const result = await transport.withInvocation(argumentsObject, {
              taskId: api.taskId, conversationId: api.conversationId, callId: api.callId,
            }, async () => {
              if (tool.name === "edit_apply_guarded") {
                const decision = await transport.authorizeTool(argumentsObject, context.abortSignal);
                if (!decision.approved) {
                  return { content: [{ type: "text" as const, text: decision.message }], isError: true };
                }
              }
              return client.callTool(tool.name, argumentsObject, { signal: context.abortSignal });
            });
            gate.assertReady(lease);
            return { output: toLlmContent(result), isError: result.isError === true };
          },
        }));
        registry.install({ name: "bk-cat-native", tools });
        const context = withAbortSignal(gate.signal, BACKGROUND_CONTEXT);
        const ownedStorage = guardedStorage(storage, gate, lease);
        if (grant.discardConversationIds !== undefined) {
          // Original Harness.open itself persists restart reconciliation. Read
          // every live record before allowing that write to a shared store.
          await reviewDiscardStorage(ownedStorage, grant.discardConversationIds, gate, lease, context);
        }
        if (recovery !== undefined) await reviewResumeStorage(ownedStorage, recovery, gate, lease, context);
        harness = await Harness.open(ownedStorage, {
          models: nativeModels(() => taskConfiguration.current, ports, gate, lease),
          registry,
          settings: {
            stream: { transport: "sse", maxRetries: 0 },
            retry: { enabled: false, maxRetries: 0 },
            compaction: { enabled: true, backgroundTokens: 0 },
            afterFailedRun: "keep",
            toolExecution: "sequential",
          },
          // Sleeping-task wakeups are reported to the host, never used
          // here to resume Pi after backgrounding or component rebuild.
          onWake: () => {},
          setTimer: (callback, delayMs) => {
            const timer = setTimeout(() => {
              try { gate.assertReady(lease); } catch { return; }
              callback();
            }, delayMs);
            return () => clearTimeout(timer);
          },
        }, context);
        const opened = harness;
        gate.assertStorageWrite(lease);
        if (recovery !== undefined) {
          reviewResumeInspection(await opened.inspect(context), recovery);
          gate.assertStorageWrite(lease);
        }
        if (grant.discardConversationIds !== undefined) {
          await discardOriginalWork(opened, grant.discardConversationIds, gate, lease, context);
          await reviewDiscardStorage(ownedStorage, grant.discardConversationIds, gate, lease, context, true);
          gate.assertStorageWrite(lease);
        }
        return {
          async inspect() {
            gate.assertOwned(lease);
            const result = await opened.inspect(context);
            gate.assertOwned(lease);
            return result;
          },
          async observeConversation(conversationId, listener) {
            gate.assertOwned(lease);
            return opened.observeConversation(id(conversationId) as ConversationId, {
              advance(call) {
                try { gate.assertReady(lease); } catch { return; }
                if (call.kind === "value") listener({ kind: "value", value: call.value });
                else listener({ kind: "failed", message: call.error instanceof Error ? call.error.message : String(call.error) });
              },
              end() {
                try { gate.assertOwned(lease); } catch { return; }
                listener({ kind: "ended" });
              },
            }, context);
          },
          async run(action, raw) {
            gate.assertReady(lease);
            return runOriginal(opened, ownedStorage, action, raw, options, taskConfiguration, tools, nativeTask);
          },
          async close() {
            const results = await Promise.allSettled([opened.close(BACKGROUND_CONTEXT), client.close()]);
            const failures = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
            if (failures.length) throw new AggregateError(failures.map((failure) => failure.reason), "Pi cleanup failed");
          },
        };
      } catch (error) {
        await Promise.allSettled([
          harness ? harness.close(BACKGROUND_CONTEXT) : storage ? storage.close(BACKGROUND_CONTEXT) : database.close(),
          client.close(),
        ]);
        if (grant.discardConversationIds !== undefined && !(error instanceof RuntimeRefused)) {
          throw new RuntimeRefused("agent.errorRecoveryChanged");
        }
        throw error;
      }
    },
  };
}

/** Direct calls into original Pi. No custom scheduling, retry or task loop. */
async function runOriginal(
  harness: Harness, storage: Storage, action: SchedulingAction, raw: unknown, options: EngineOpenOptions,
  taskConfiguration: { current: ModelConfiguration }, tools: readonly ToolRegistration[], nativeTask: NativeTaskState,
): Promise<unknown> {
  const { gate, lease } = options;
  const context = withAbortSignal(gate.signal, BACKGROUND_CONTEXT);
  const input = raw === undefined ? {} : argumentObject(raw);
  gate.assertReady(lease);
  switch (action) {
    case "nativeResume": {
      const admitted = nativeResumeInput(raw);
      const scope = options.grant.resumeTask === undefined ? undefined : nativeRecoveryScope(options.grant.resumeTask);
      if (!scope || nativeTask.active || nativeTask.recoveryConsumed) throw new RuntimeRefused("agent.errorRecoveryChanged");
      assertRecoveryIdentity(scope, admitted);
      reviewResumeInspection(await harness.inspect(context), scope);
      gate.assertReady(lease);
      const configuration = nativeModelConfiguration(admitted.modelConfiguration);
      const model = configuration.native;
      if (!model) throw new RuntimeRefused("agent.errorRecoverySnapshotInvalid");
      const conversation = await harness.conversation(admitted.conversationId as ConversationId, context);
      gate.assertReady(lease);
      if (!conversation) throw new RuntimeRefused("agent.errorRecoveryScope");
      // Original history/checkpoints remain untouched. Configure only the
      // current native authority context and the captured task's languages.
      await conversation.configure({
        model:{provider:model.id,modelId:model.model},
        instructions:taskInstructions(admitted) + "\nNative recovery facts for this original task:\n" + admitted.nativeContext,
        maxTurns:null,
        tools:tools.filter((tool) => admitted.mode === "edit" || tool.name !== "edit_apply_guarded"),
        compaction:{enabled:true,backgroundTokens:0},
      }, context);
      gate.assertReady(lease);
      taskConfiguration.current = configuration;
      nativeTask.recoveryConsumed = true;
      nativeTask.active = {...admitted, resumed:true};
      gate.authorizeNetwork(lease, admitted.networkPermit);
      try {
        // This is the pinned scheduler's own continuation. Unsafe persisted
        // tool intent receives its original interruption outcome, never a rerun.
        harness.resume();
        return {resumed:true, taskId:admitted.taskId, conversationId:admitted.conversationId, submissionId:admitted.submissionId};
      } catch (error) {
        gate.finishNetwork(admitted.networkPermit);
        nativeTask.active = undefined;
        throw error;
      }
    }

    case "nativeSubmit": {
      const input = nativeTaskInput(raw);
      if (nativeTask.active) throw new RuntimeRefused("another native task is active");
      const configuration = input.modelConfiguration === undefined ? taskConfiguration.current
        : nativeModelConfiguration(input.modelConfiguration);
      const model = configuration.native;
      if (!model || !options.ports.tasks) throw new RuntimeRefused("native model settings or task binding are unavailable");
      if (model.protocol === "openai-responses" && model.maxOutputTokens < 16) {
        throw new RuntimeRefused("the responses protocol requires an output limit of at least 16 tokens");
      }
      nativeTask.active = { ...input };
      taskConfiguration.current = configuration;
      try {
        // Pi.submit resumes the shared scheduler. Do not use a new input to
        // reactivate interrupted tasks anywhere in this store.
        const unfinished = await harness.inspect(context);
        gate.assertReady(lease);
        if (unfinished.tasks.length || unfinished.submissions.length) {
          throw new RuntimeRefused("unfinished durable work requires explicit native reconciliation");
        }
        const languages = taskLanguages(input.languages);
        const instructions = taskInstructions(input);
        const change: AgentChange = {
          // Null also clears a four-turn cap persisted by an older conversation.
          // The original Pi scheduler can continue until the user stops it.
          model: { provider: model.id, modelId: model.model }, instructions, maxTurns: null,
          tools: tools.filter((tool) => input.mode === "edit" || tool.name !== "edit_apply_guarded"),
          // Original Pi uses contextWindow <= 0 as unknown; no invented
          // capacity or product-wide request/token budget is introduced.
          compaction: { enabled: true, backgroundTokens: 0 },
        };
        const conversation = input.conversationId === undefined
          ? await harness.createConversation({ ownership: { kind: "ownerless" }, agent: change }, context)
          : await harness.conversation(input.conversationId as ConversationId, context);
        gate.assertReady(lease);
        if (!conversation) throw new RuntimeRefused("the saved conversation no longer exists");
        if (input.conversationId !== undefined) await conversation.configure(change, context);
        gate.assertReady(lease);
        await options.ports.tasks.bind(lease, input.taskId, conversation.id, gate.signal);
        gate.assertReady(lease);
        nativeTask.active = { ...input, conversationId: conversation.id };
        gate.authorizeNetwork(lease, input.networkPermit);
        const submission = await conversation.submit({
          type: "input", requestId: input.taskId, content: input.input, whenBusy: "reject",
          // Original Pi persists display data beside the full model input in
          // the same entry. It never becomes instructions or edit authority.
          ...(input.displayText === undefined ? {} : {
            entry: { kind: "bk-cat.user", data: { schemaVersion: 2, text: input.displayText, languages: { ...languages } } },
          }),
        }, context);
        gate.assertReady(lease);
        nativeTask.active = { ...nativeTask.active, submissionId: submission.id };
        return { taskId: input.taskId, conversationId: conversation.id, submissionId: submission.id };
      } catch (error) {
        gate.finishNetwork(input.networkPermit);
        nativeTask.active = undefined;
        throw error;
      }
    }
    case "nativeWait": {
      const active = nativeTask.active;
      if (!active || input.taskId !== active.taskId || active.submissionId === undefined || active.conversationId === undefined) {
        throw new RuntimeRefused("native task completion does not match the admitted submission");
      }
      try {
        const submission = await harness.submission(active.submissionId as SubmissionId, context);
        gate.assertReady(lease);
        if (!submission) throw new RuntimeRefused("native durable submission is missing");
        const result = await submission.wait(context);
        gate.assertReady(lease);
        if (active.resumed) {
          // Finish original local drain/compaction cleanup before retiring the
          // restored task's authority. No replacement input is submitted.
          await harness.waitForIdle(context);
          gate.assertReady(lease);
        }
        if (result.conversationId !== active.conversationId || result.requestId !== active.taskId) {
          throw new RuntimeRefused("durable submission identity changed");
        }
        let text = "";
        if (result.status === "done" && result.answer !== undefined) {
          // The original Harness has no entry reader. Read the exact committed
          // answer through the same fenced original storage, scoped to the
          // admitted conversation, and reject even a visible ancestor's entry.
          const stored = await storage.entry(active.conversationId as ConversationId, result.answer, context);
          gate.assertReady(lease);
          const answer = stored?.entry;
          if (!answer || answer.id !== result.answer || answer.conversationId !== active.conversationId) {
            throw new RuntimeRefused("durable answer is missing");
          }
          text = (answer.model ?? []).filter((message) => message.role === "assistant")
            .flatMap((message) => message.content.filter((part) => part.type === "text").map((part) => part.text)).join("\n");
          // JSON can expand control characters to six bytes; this bounded
          // answer still fits the 2 MiB native frame with identity metadata.
          if (new TextEncoder().encode(text).byteLength > 256 * 1024) {
            throw new RuntimeRefused("durable answer exceeds the native message limit");
          }
        }
        const allowedReasons = new Set(["aborted", "no_model", "turn_limit", "error", "failed", "interrupted", "restart_limit", "tool_error", "tool_errors", "length"]);
        return {
          taskId: active.taskId, conversationId: active.conversationId, submissionId: active.submissionId,
          status: result.status, text,
          ...(result.status === "unanswered" ? { reason: allowedReasons.has(result.reason) ? result.reason : "failed" } : {}),
        };
      } finally {
        gate.finishNetwork(active.networkPermit);
        nativeTask.active = undefined;
      }
    }
    case "resume": harness.resume(); return undefined;
    case "waitForIdle": return harness.waitForIdle(context);
    case "createConversation": {
      const created = await harness.createConversation(conversationOptions(input), context);
      gate.assertReady(lease);
      return { id: created.id };
    }
    case "abortTask": return harness.abortTask(id(input.taskId) as TaskId, context);
    case "waitForTask": return harness.waitForTask(id(input.taskId) as TaskId, context);
    case "abortSubmission": return harness.abortSubmission(id(input.submissionId) as SubmissionId, context);
    case "waitSubmission": {
      const submission = await harness.submission(id(input.submissionId) as SubmissionId, context);
      gate.assertReady(lease);
      if (!submission) throw new RuntimeRefused("submission no longer exists");
      return submission.wait(context);
    }
    case "signal":
      // Signal definitions are executable application registrations. None
      // is exposed by this first profile; do not accept a forged JSON token.
      throw new RuntimeRefused("no native signal definition has been reviewed for this runtime");
    default: {
      const conversation = await harness.conversation(id(input.conversationId) as ConversationId, context);
      gate.assertReady(lease);
      if (!conversation) throw new RuntimeRefused("conversation no longer exists");
      if (action === "submit") {
        const submission = submissionDraft(input.submission);
        const result = await conversation.submit(submission, context);
        gate.assertReady(lease);
        return { id: result.id };
      }
      if (action === "compact") return conversation.compact(optionalText(input.instructions), context);
      if (action === "abortConversation") return conversation.abort(context);
      if (action === "resetConversation") return conversation.reset(optionalText(input.handoff), context);
      throw new RuntimeRefused("unreviewed Pi scheduling action");
    }
  }
}

function taskInstructions(input: Pick<NativeTaskInput, "mode" | "languages">): string {
  const base = input.mode === "script"
    ? "Create or revise a writing candidate as requested. This task may read information but must not change the editing project."
    : input.mode === "edit"
      ? "在当前原生工程中按用户明确选择的文案版本和任务要求剪辑。先读取工程和时间线，仅使用受保护编辑工具；工具拒绝时说明实际原因，不得宣称执行成功。"
      : "回答用户关于当前工程的问题。当前任务只允许读取信息，不得修改剪辑工程。";
  return base + "\n" + languageInstructions(taskLanguages(input.languages), input.mode === "script");
}

interface NativeTaskState {
  active?: NativeTaskInput & { conversationId?: number; submissionId?: number; resumed?: boolean };
  recoveryConsumed?: boolean;
}

function id(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new RuntimeRefused("invalid Pi object identity");
  return value;
}
function optionalText(value: unknown): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string") throw new RuntimeRefused("a string is required");
  return value;
}
