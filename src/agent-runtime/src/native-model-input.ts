// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
// Input admission stays independent of original model SDK imports.
import { RuntimeRefused } from "./gate.ts";

export type NativeProtocol = "openai-completions" | "openai-responses" | "anthropic-messages" | "google-generative-ai";

/** Non-secret configuration copied by native code, never supplied by a tool. */
export interface NativeModelConfiguration {
  readonly id: string;
  readonly version: string;
  readonly configurationVersion: string;
  readonly protocol: NativeProtocol;
  readonly providerId?: string;
  readonly endpoint: string;
  readonly model: string;
  readonly requestTimeoutMs: number;
  readonly maxOutputTokens: number;
  readonly contextWindow?: number | null;
  readonly contextSource?: string | null;
}

export function validateNativeModelConfiguration(raw: unknown): NativeModelConfiguration {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new RuntimeRefused("native model configuration is invalid");
  const value = raw as Record<string, unknown>;
  const keys = ["id", "version", "configurationVersion", "protocol", "endpoint", "model", "requestTimeoutMs", "maxOutputTokens"];
  if (!keys.every((key) => key in value) || Object.keys(value).some((key) => ![...keys, "providerId", "contextWindow", "contextSource"].includes(key))) {
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
  if (typeof value.protocol !== "string" || !["openai-completions", "openai-responses", "anthropic-messages", "google-generative-ai"].includes(value.protocol)) {
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
  if (value.providerId != null && (typeof value.providerId !== "string" || !["deepseek", "openai", "anthropic", "google", "qwen", "doubao", "zhipu", "moonshot", "minimax", "siliconflow", "openrouter", "custom"].includes(value.providerId))) {
    throw new RuntimeRefused("native provider identity is invalid");
  }
  if (value.contextWindow != null && (!Number.isSafeInteger(value.contextWindow) || Number(value.contextWindow) < 1 || Number(value.contextWindow) > 100000000)) {
    throw new RuntimeRefused("native model context metadata is invalid");
  }
  if (value.contextSource != null && (typeof value.contextSource !== "string" || !value.contextSource || value.contextSource.length > 2048)) {
    throw new RuntimeRefused("native model context source is invalid");
  }
  if ((value.contextWindow != null) !== (value.contextSource != null)) throw new RuntimeRefused("native context capacity requires provenance");
  return Object.freeze({ ...value, providerId: value.providerId ?? "custom" }) as unknown as NativeModelConfiguration;
}
