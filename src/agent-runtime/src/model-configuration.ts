// SPDX-License-Identifier: AGPL-3.0-or-later

import { createProvider } from "@earendil-works/pi-ai/models";
import { envApiKeyAuth, type Model, type ProviderStreams } from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/api/openai-completions.lazy";
import { openAIResponsesApi } from "@earendil-works/pi-ai/api/openai-responses.lazy";
import { googleGenerativeAIApi } from "@earendil-works/pi-ai/api/google-generative-ai.lazy";
import { anthropicMessagesApi } from "@earendil-works/pi-ai/api/anthropic-messages.lazy";
import { RuntimeRefused } from "./gate.ts";
import pinnedContext from "./pinned-model-context.json" with { type: "json" };
import type { ModelConfiguration } from "./models.ts";

import { validateNativeModelConfiguration, type NativeModelConfiguration, type NativeProtocol } from "./native-model-input.ts";
export { validateNativeModelConfiguration, type NativeModelConfiguration, type NativeProtocol } from "./native-model-input.ts";

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
  const checked = validateNativeModelConfiguration(raw);
  const fallback = checked.contextWindow == null ? pinnedCapacity(checked) : undefined;
  const configured: NativeModelConfiguration = fallback
    ? Object.freeze({ ...checked, contextWindow: fallback.contextWindow, contextSource: fallback.source }) : checked;
  // Match native's exact destination construction even when a saved base
  // contains multiple trailing slashes; do not change its credential scope.
  const serviceBase = configured.endpoint.replace(/\/+$/u, "");
  const adapters: Record<NativeProtocol, () => ProviderStreams> = {
    "openai-completions": openAICompletionsApi,
    "openai-responses": openAIResponsesApi,
    "anthropic-messages": anthropicMessagesApi,
    "google-generative-ai": googleGenerativeAIApi,
  };
  // Only the four reviewed native function tools are available. Do not let
  // provider/model-name inference turn them into server-side tool discovery,
  // deferred tool changes, grammar execution, threads or fallback models.
  // Routing objects are deliberately absent: even {} requests routing fields
  // in the original completions adapter. Native rejects such fields as well.
  const compatibility: Partial<Record<NativeProtocol, NonNullable<Model<NativeProtocol>["compat"]>>> = {
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
  // Official DeepSeek enables thinking by default. The original adapter needs
  // this capability to replay an empty reasoning_content field after a tool
  // call. Keep the explicit compatibility settings above: this does not send
  // thinking/reasoning_effort options or infer a vendor from a model name.
  // Legacy custom connections retain their provider identity and key scope.
  const deepseekReasoningReplay = configured.protocol === "openai-completions"
    && (serviceBase === "https://api.deepseek.com" || serviceBase === "https://api.deepseek.com/v1");
  const model: Model<NativeProtocol> = {
    id: configured.model, name: configured.model, api: configured.protocol,
    provider: configured.id, baseUrl: serviceBase, input: ["text"], reasoning: deepseekReasoningReplay,
    compat: compatibility[configured.protocol],
    // Unknown capacity stays explicitly unknown in native metadata. This value is
    // an unknown sentinel for the original model registry, not verified
    // provider capacity and never grounds automatic compaction.
    contextWindow: configured.contextWindow ?? 0,
    maxTokens: configured.maxOutputTokens,
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
    contextKnown: configured.contextWindow != null, contextSource: configured.contextSource ?? undefined,
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

/** Offline metadata authored in the immutable original Pi commit. Never infer
 * a provider from a model name or apply official capacity to a custom proxy.
 * Data absent from that commit stays unknown instead of hydrating live data.
 */
function pinnedCapacity(configuration: NativeModelConfiguration) {
  const provider = configuration.providerId ?? "custom";
  const official: Readonly<Record<string, readonly [string, string, NativeProtocol]>> = {
    deepseek: ["deepseek", "https://api.deepseek.com", "openai-completions"],
    openai: ["openai", "https://api.openai.com/v1", "openai-responses"],
    anthropic: ["anthropic", "https://api.anthropic.com", "anthropic-messages"],
    google: ["google", "https://generativelanguage.googleapis.com/v1beta", "google-generative-ai"],
    moonshot: ["moonshotai-cn", "https://api.moonshot.cn/v1", "openai-completions"],
    minimax: ["minimax-cn", "https://api.minimax.cn/v1", "openai-completions"],
    openrouter: ["openrouter", "https://openrouter.ai/api/v1", "openai-completions"],
  };
  const admitted = official[provider];
  if (!admitted || configuration.protocol !== admitted[2]
    || new URL(configuration.endpoint).href.replace(/\/+$/u, "") !== admitted[1]) return undefined;
  return pinnedContext.models.find((entry) => entry.provider === admitted[0] && entry.model === configuration.model
    && entry.protocol === configuration.protocol && entry.endpoint === admitted[1]);
}
