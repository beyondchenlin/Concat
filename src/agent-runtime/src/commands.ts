// SPDX-License-Identifier: AGPL-3.0-or-later

import type { AgentChange, ConversationCreateOptions, SubmissionDraft } from "@earendil-works/pi-durable";
import { RuntimeRefused } from "./gate.ts";

/** Native UI conversations are independent. Executable callbacks and
 * registry objects never enter the bridge as request arguments. */
export function conversationOptions(input: Record<string, unknown>): ConversationCreateOptions {
  allowKeys(input, ["ownership", "agent"]);
  if (input.ownership !== undefined) {
    const ownership = argumentObject(input.ownership);
    allowKeys(ownership, ["kind"]);
    if (ownership.kind !== "ownerless") throw new RuntimeRefused("native UI conversations must be ownerless");
  }
  if (input.agent === undefined) return { ownership: { kind: "ownerless" } };
  const agent = argumentObject(input.agent);
  allowKeys(agent, ["model", "instructions", "maxTurns"]);
  const change: { model?: AgentChange["model"]; instructions?: string; maxTurns?: number } = {};
  if (agent.model !== undefined) {
    const model = argumentObject(agent.model);
    allowKeys(model, ["provider", "modelId"]);
    if (typeof model.provider !== "string" || !model.provider || typeof model.modelId !== "string" || !model.modelId) {
      throw new RuntimeRefused("an explicitly selected provider and model are required");
    }
    change.model = { provider: model.provider, modelId: model.modelId };
  }
  if (agent.instructions !== undefined) {
    if (typeof agent.instructions !== "string") throw new RuntimeRefused("instructions must be text");
    change.instructions = agent.instructions;
  }
  if (agent.maxTurns !== undefined) {
    if (typeof agent.maxTurns !== "number" || !Number.isSafeInteger(agent.maxTurns) || agent.maxTurns < 1) {
      throw new RuntimeRefused("maxTurns must be a positive safe integer");
    }
    change.maxTurns = agent.maxTurns;
  }
  return { ownership: { kind: "ownerless" }, agent: change };
}

/** This profile admits native chat input or an explicit continuation.
 * Passive arbitrary entry writes could forge assistant/tool history. */
export function submissionDraft(value: unknown): SubmissionDraft {
  const input = argumentObject(value);
  allowKeys(input, ["type", "requestId", "content", "whenBusy"]);
  if (typeof input.requestId !== "string" || !input.requestId.trim()) {
    throw new RuntimeRefused("a durable submission deduplication key is required");
  }
  if (input.type === "continue") {
    if ("content" in input || "whenBusy" in input) throw new RuntimeRefused("continuation must not contain a new message");
    return { type: "continue", requestId: input.requestId };
  }
  if (input.type !== "input" || typeof input.content !== "string" || !input.content.trim()) {
    throw new RuntimeRefused("a nonempty native chat message is required");
  }
  if (input.whenBusy !== undefined && input.whenBusy !== "reject") {
    throw new RuntimeRefused("this profile does not enqueue a message behind an active run");
  }
  return { type: "input", requestId: input.requestId, content: input.content, whenBusy: "reject" };
}

export function argumentObject(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new RuntimeRefused("an argument object is required");
  return value as Record<string, unknown>;
}

function allowKeys(value: Record<string, unknown>, allowed: readonly string[]): void {
  for (const key of Object.keys(value)) if (!allowed.includes(key)) throw new RuntimeRefused(`unreviewed argument ${key}`);
}

export interface NativeTaskInput {
  readonly taskId: string;
  readonly networkPermit: string;
  readonly conversationId?: number;
  readonly input: string;
  readonly mode: "chat" | "script" | "edit";
}

export function nativeTaskInput(value: unknown): NativeTaskInput {
  const input = argumentObject(value);
  allowKeys(input, ["taskId", "networkPermit", "conversationId", "input", "mode"]);
  if (typeof input.taskId !== "string" || !input.taskId || input.taskId.length > 256
    || typeof input.networkPermit !== "string" || !input.networkPermit || input.networkPermit.length > 256
    || typeof input.input !== "string" || !input.input.trim() || new TextEncoder().encode(input.input).byteLength > 512 * 1024
    || !["chat", "script", "edit"].includes(String(input.mode))
    || (input.conversationId !== undefined && (typeof input.conversationId !== "number" || !Number.isSafeInteger(input.conversationId) || input.conversationId < 0))) {
    throw new RuntimeRefused("native task input is invalid");
  }
  return Object.freeze({ ...input }) as unknown as NativeTaskInput;
}
