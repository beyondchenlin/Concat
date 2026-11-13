// SPDX-License-Identifier: AGPL-3.0-or-later

import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import {
  Harness, createRegistry, type ConversationId, type SubmissionId, type TaskId,
  type ToolRegistration,
} from "@earendil-works/pi-durable";
import { CURRENT_SQLITE_SCHEMA_VERSION, SqliteStorage } from "@earendil-works/pi-durable/storage/sqlite";
// The pinned package root also re-exports its Node stdio transport. Use
// these reviewed original source modules, without copying a client or
// substituting browser stubs for Node's process/filesystem modules.
import { McpClient } from "@bk-cat/pi-reference/mcp/client";
import { normalizeMcpInputSchema } from "@bk-cat/pi-reference/mcp/input-schema";
import { toLlmContent } from "@bk-cat/pi-reference/mcp/content";
import { RuntimeRefused } from "./gate.ts";
import { argumentObject, conversationOptions, submissionDraft } from "./commands.ts";
import { nativeModels, type ModelConfiguration } from "./models.ts";
import { EMBEDDED_TOOLS, guardedDatabase, guardedStorage, NativeMcpTransport } from "./native-adapters.ts";
import type { EngineOpenOptions, PiEngine, PiEngineFactory, SchedulingAction } from "./ports.ts";

/** Builds only the original pinned Pi Harness and portable SqliteStorage. */
export function piEngineFactory(configuration: ModelConfiguration): PiEngineFactory {
  return {
    async open(options: EngineOpenOptions): Promise<PiEngine> {
      const { ports, gate, lease, grant } = options;
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
            const argumentsObject = { ...args as Record<string, unknown> };
            const result = await transport.withInvocation(argumentsObject, {
              taskId: api.taskId, conversationId: api.conversationId, callId: api.callId,
            }, () => client.callTool(tool.name, argumentsObject, { signal: context.abortSignal }));
            gate.assertReady(lease);
            return { output: toLlmContent(result), isError: result.isError === true };
          },
        }));
        registry.install({ name: "bk-cat-native", tools });
        const context = withAbortSignal(gate.signal, BACKGROUND_CONTEXT);
        harness = await Harness.open(guardedStorage(storage, gate, lease), {
          models: nativeModels(configuration, ports, gate, lease),
          registry,
          settings: {
            stream: { transport: "sse", maxRetries: 0 },
            retry: { enabled: false, maxRetries: 0 },
            compaction: { enabled: false },
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
            return runOriginal(opened, action, raw, options);
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
        throw error;
      }
    },
  };
}

/** Direct calls into original Pi. No custom scheduling, retry or task loop. */
async function runOriginal(harness: Harness, action: SchedulingAction, raw: unknown, options: EngineOpenOptions): Promise<unknown> {
  const { gate, lease } = options;
  const context = withAbortSignal(gate.signal, BACKGROUND_CONTEXT);
  const input = raw === undefined ? {} : argumentObject(raw);
  gate.assertReady(lease);
  switch (action) {
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

function id(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new RuntimeRefused("invalid Pi object identity");
  return value;
}
function optionalText(value: unknown): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string") throw new RuntimeRefused("a string is required");
  return value;
}
