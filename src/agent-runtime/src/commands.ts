// SPDX-License-Identifier: AGPL-3.0-or-later

import type { AgentChange, ConversationCreateOptions, SubmissionDraft } from "@earendil-works/pi-durable";
import { RuntimeRefused } from "./gate.ts";
import { taskLanguages, type TaskLanguages } from "./languages.ts";
import { validateNativeModelConfiguration, type NativeModelConfiguration } from "./native-model-input.ts";

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
  /** Native-captured editor text for display only; never model or tool authority. */
  readonly displayText?: string;
  readonly mode: "chat" | "script" | "edit";
  readonly languages?: TaskLanguages;
  readonly modelConfiguration?: NativeModelConfiguration;
}

export function nativeTaskInput(value: unknown): NativeTaskInput {
  const input = argumentObject(value);
  allowKeys(input, ["taskId", "networkPermit", "conversationId", "input", "displayText", "mode", "languages", "modelConfiguration"]);
  if (typeof input.taskId !== "string" || !input.taskId || input.taskId.length > 256
    || typeof input.networkPermit !== "string" || !input.networkPermit || input.networkPermit.length > 256
    || typeof input.input !== "string" || !input.input.trim() || new TextEncoder().encode(input.input).byteLength > 512 * 1024
    || (input.displayText !== undefined && (typeof input.displayText !== "string" || !input.displayText.trim()
      || new TextEncoder().encode(input.displayText).byteLength > 256 * 1024))
    || !["chat", "script", "edit"].includes(String(input.mode))
    || (input.conversationId !== undefined && (typeof input.conversationId !== "number" || !Number.isSafeInteger(input.conversationId) || input.conversationId < 0))) {
    throw new RuntimeRefused("native task input is invalid");
  }
  return Object.freeze({ ...input,
    ...(input.languages === undefined ? {} : { languages: taskLanguages(input.languages) }),
    ...(input.modelConfiguration === undefined ? {} : { modelConfiguration: validateNativeModelConfiguration(input.modelConfiguration) }),
  }) as unknown as NativeTaskInput;
}

/** Native-confirmed original submission, including its two language snapshots. */
export interface NativeRecoveryScope {
  readonly taskId: string;
  readonly conversationId: number;
  readonly submissionId: number;
  readonly taskIds: readonly number[];
  readonly mode: NativeTaskInput["mode"];
  readonly languages: TaskLanguages;
  readonly modelConfiguration: NativeModelConfiguration;
}
export interface NativeResumeInput extends NativeTaskInput {
  readonly conversationId: number;
  readonly submissionId: number;
  readonly languages: TaskLanguages;
  readonly modelConfiguration: NativeModelConfiguration;
  /** Fresh native project/version and reconciled receipt facts, not user text. */
  readonly nativeContext: string;
}
export interface NativeResumeAccepted {
  readonly resumed: true;
  readonly taskId: string;
  readonly conversationId: number;
  readonly submissionId: number;
}

export function nativeRecoveryScope(value: unknown): NativeRecoveryScope {
  const input = argumentObject(value);
  allowKeys(input, ["taskId", "conversationId", "submissionId", "taskIds", "mode", "languages", "modelConfiguration"]);
  if (typeof input.taskId !== "string" || !input.taskId || input.taskId.length > 256
    || !["chat", "script", "edit"].includes(String(input.mode))
    || [input.conversationId, input.submissionId].some((id) => typeof id !== "number" || !Number.isSafeInteger(id) || id < 0)
    || !Array.isArray(input.taskIds) || new Set(input.taskIds).size !== input.taskIds.length
    || Array.from(input.taskIds).some((id) => typeof id !== "number" || !Number.isSafeInteger(id) || id < 0)
    || input.languages === undefined || input.modelConfiguration === undefined) {
    throw new RuntimeRefused("agent.errorRecoverySnapshotInvalid");
  }
  return Object.freeze({ ...input, taskIds:Object.freeze([...input.taskIds]),
    languages:taskLanguages(input.languages), modelConfiguration:validateNativeModelConfiguration(input.modelConfiguration),
  }) as unknown as NativeRecoveryScope;
}
export function nativeResumeInput(value: unknown): NativeResumeInput {
  const input = argumentObject(value);
  const { submissionId, nativeContext, ...base } = input;
  const task = nativeTaskInput(base);
  if (task.conversationId === undefined || task.languages === undefined || task.modelConfiguration === undefined
    || typeof submissionId !== "number" || !Number.isSafeInteger(submissionId) || submissionId < 0
    || typeof nativeContext !== "string" || !nativeContext || new TextEncoder().encode(nativeContext).length > 256 * 1024) {
    throw new RuntimeRefused("agent.errorRecoverySnapshotInvalid");
  }
  return Object.freeze({...task, submissionId, nativeContext}) as NativeResumeInput;
}
export function assertRecoveryIdentity(scope: NativeRecoveryScope, input: NativeResumeInput): void {
  if (scope.taskId !== input.taskId || scope.conversationId !== input.conversationId
    || scope.submissionId !== input.submissionId || scope.mode !== input.mode
    || JSON.stringify(scope.languages) !== JSON.stringify(input.languages)
    || JSON.stringify(scope.modelConfiguration) !== JSON.stringify(input.modelConfiguration)) {
    throw new RuntimeRefused("agent.errorRecoveryChanged");
  }
}


/** Native acknowledgement of this project's facts only; no scheduler grant. */
export interface NativeRecoveryReviewed {
  readonly reviewed:true;
  readonly remainingOtherOperations:number;
}
