// SPDX-License-Identifier: AGPL-3.0-or-later

import { createProvider, envApiKeyAuth, type Model, type ProviderStreams } from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/api/openai-completions.lazy";
import { openAIResponsesApi } from "@earendil-works/pi-ai/api/openai-responses.lazy";
import { anthropicMessagesApi } from "@earendil-works/pi-ai/api/anthropic-messages.lazy";
import { RuntimeRefused } from "./gate.ts";
import type { ModelConfiguration } from "./models.ts";

export type NativeProtocol = "openai-completions" | "openai-responses" | "anthropic-messages";

/** Non-secret configuration copied by native code, never supplied by a tool. */
export interface NativeModelConfiguration {
  readonly id: string;
  readonly version: string;
  readonly configurationVersion: string;
  readonly protocol: NativeProtocol;
  readonly endpoint: string;
  readonly model: string;
  readonly requestTimeoutMs: number;
  readonly maxOutputTokens: number;
}

// This literal is not a credential and grants no network authority. It only
// satisfies the original SDK's required API-key shape; the native transport
// removes auth headers and injects the actual system credential after checks.
// The sk- prefix also prevents the original OpenAI adapter from treating a
// native channel named "openai" as a ChatGPT OAuth token and dropping limits.
export const NATIVE_CREDENTIAL_PLACEHOLDER = "sk-bk-cat-native-credential-placeholder";

export function nativeModelConfiguration(raw: unknown): ModelConfiguration {
  const denied = async () => { throw new RuntimeRefused("model settings belong to the native application"); };
  if (raw === undefined || raw === null) return {
    providers: [], credentials: { read: denied, list: denied, modify: denied, delete: denied },
  };
  const configured = validateNativeModelConfiguration(raw);
  // Match native's exact destination construction even when a saved base
  // contains multiple trailing slashes; do not change its credential scope.
  const serviceBase = configured.endpoint.replace(/\/+$/u, "");
  const adapters: Record<NativeProtocol, () => ProviderStreams> = {
    "openai-completions": openAICompletionsApi,
    "openai-responses": openAIResponsesApi,
    "anthropic-messages": anthropicMessagesApi,
  };
  // Only the four reviewed native function tools are available. Do not let
  // provider/model-name inference turn them into server-side tool discovery,
  // deferred tool changes, grammar execution, threads or fallback models.
  // Routing objects are deliberately absent: even {} requests routing fields
  // in the original completions adapter. Native rejects such fields as well.
  const compatibility: Record<NativeProtocol, NonNullable<Model<NativeProtocol>["compat"]>> = {
    "openai-completions": {
      supportsReasoningEffort: false, thinkingFormat: "openai", supportsThinkingTokenBudget: false,
      supportsOpenAIGrammarTools: false, supportsStrictMode: false,
      supportsMidConvoSystemMessages: false, supportsMidConvoToolAdditions: false,
      sendSessionAffinityHeaders: false, promptCacheKeyMode: "disabled", supportsLongCacheRetention: false,
      zaiToolStream: false,
    },
    "openai-responses": {
      supportsOpenAIGrammarTools: false, supportsStrictMode: false,
      supportsAdditionalTools: false, supportsToolSearch: false,
      supportsMidConvoSystemMessages: false, supportsMidConvoEffort: false,
      promptCacheKeyMode: "disabled", supportsLongCacheRetention: false,
      supportsExplicitPromptCacheMode: false, supportsMaxOutputTokens: true,
    },
    "anthropic-messages": {
      supportsMidConvoSystemMessages: false, supportsMidConvoToolChanges: false,
      supportsMidConvoEffort: false, supportsStrictTools: false,
      supportsMessageThreads: false, allowedFallbackModels: [], forceAdaptiveThinking: false,
      sendSessionAffinityHeaders: false, supportsLongCacheRetention: false,
      supportsCacheControlOnTools: false,
    },
  };
  const model: Model<NativeProtocol> = {
    id: configured.model, name: configured.model, api: configured.protocol,
    provider: configured.id, baseUrl: serviceBase, input: ["text"], reasoning: false,
    compat: compatibility[configured.protocol],
    // Pi requires numerical metadata. This is an application admission limit,
    // not a claim about a service's context window. Automatic compaction is off.
    contextWindow: Math.max(32768, configured.maxOutputTokens * 2), maxTokens: configured.maxOutputTokens,
    // Unknown pricing is not a zero-cost promise. Native UI must not display
    // these structural Pi counters as a quote or use them to approve spending.
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  };
  const provider = createProvider({
    id: configured.id, name: configured.id, baseUrl: serviceBase,
    auth: { apiKey: envApiKeyAuth("Native scoped credential", []) },
    models: [model], api: adapters[configured.protocol](),
  });
  return {
    providers: [provider], native: configured, pricingKnown: false,
    credentials: {
      async read(providerId, options) {
        options?.signal?.throwIfAborted();
        if (providerId !== configured.id) throw new RuntimeRefused("model channel is not configured");
        return { type: "api_key", key: NATIVE_CREDENTIAL_PLACEHOLDER };
      },
      list: denied, modify: denied, delete: denied,
    },
  };
}

export function validateNativeModelConfiguration(raw: unknown): NativeModelConfiguration {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new RuntimeRefused("native model configuration is invalid");
  const value = raw as Record<string, unknown>;
  const keys = ["id", "version", "configurationVersion", "protocol", "endpoint", "model", "requestTimeoutMs", "maxOutputTokens"];
  if (Object.keys(value).length !== keys.length || Object.keys(value).some((key) => !keys.includes(key))) {
    throw new RuntimeRefused("native model configuration contains unreviewed fields");
  }
  for (const key of ["id", "model"] as const) {
    if (typeof value[key] !== "string" || !value[key] || [...value[key]].length > 256 || /\s|[\u0000-\u001f\u007f]/u.test(value[key])) {
      throw new RuntimeRefused("native model identity is invalid");
    }
  }
  for (const key of ["version", "configurationVersion"] as const) {
    if (typeof value[key] !== "string" || !/^[1-9][0-9]{0,19}$/.test(value[key]) || BigInt(value[key]) > 18446744073709551615n) {
      throw new RuntimeRefused("native configuration revision is invalid");
    }
  }
  if (!["openai-completions", "openai-responses", "anthropic-messages"].includes(String(value.protocol))) {
    throw new RuntimeRefused("model protocol is not supported");
  }
  if (typeof value.endpoint !== "string" || value.endpoint.length > 2048 || /\s|[\u0000-\u001f\u007f?#]/u.test(value.endpoint)) {
    throw new RuntimeRefused("model service address is invalid");
  }
  let endpoint: URL;
  try { endpoint = new URL(value.endpoint); } catch { throw new RuntimeRefused("model service address is invalid"); }
  const loopback = ["[::1]", "localhost"].includes(endpoint.hostname) || /^127(?:\.[0-9]{1,3}){3}$/.test(endpoint.hostname);
  if (endpoint.username || endpoint.password || endpoint.search || endpoint.hash || endpoint.port === "0"
    || (endpoint.protocol !== "https:" && !(endpoint.protocol === "http:" && loopback))) {
    throw new RuntimeRefused("model service address is not allowed");
  }
  if (!Number.isSafeInteger(value.requestTimeoutMs) || Number(value.requestTimeoutMs) < 1000 || Number(value.requestTimeoutMs) > 600000
    || !Number.isSafeInteger(value.maxOutputTokens) || Number(value.maxOutputTokens) < 1 || Number(value.maxOutputTokens) > 1000000) {
    throw new RuntimeRefused("native model limits are invalid");
  }
  return Object.freeze({ ...value }) as unknown as NativeModelConfiguration;
}
