// SPDX-License-Identifier: AGPL-3.0-or-later

/** A bounded display projection of original Pi records. It never fabricates
 * assistant text, success, prices, task state, or a second durable transcript. */
export function conversationPresentation(value: unknown): {
  messages: { role: "user" | "assistant" | "tool"; text: string }[];
  partialText: string;
  busy: boolean;
} {
  const view = object(value);
  const messages: { role: "user" | "assistant" | "tool"; text: string }[] = [];
  let remaining = 128 * 1024;
  const entries = Array.isArray(view.entries) ? view.entries : [];
  // Keep the newest messages in a bounded native bridge frame. Full entries
  // remain in the original session database; this is only the visible window.
  for (const entry of entries.slice().reverse()) {
    const record = object(entry);
    const models = record.model;
    if (!Array.isArray(models)) continue;
    const displayText = nativeUserDisplayText(record, models);
    for (const message of models.slice().reverse()) {
      const item = object(message);
      const role = item.role === "toolResult" ? "tool" : item.role;
      if (role !== "user" && role !== "assistant" && role !== "tool") continue;
      const text = (role === "user" && displayText !== undefined ? displayText : textContent(item.content))
        .slice(0, Math.min(32768, remaining));
      if (text) { messages.unshift({ role, text }); remaining -= text.length; }
      if (messages.length >= 100 || remaining <= 0) break;
    }
    if (messages.length >= 100 || remaining <= 0) break;
  }
  const live = object(object(view.docs)["pi.live"]);
  return {
    messages,
    partialText: textContent(object(object(live.generation).message).content).slice(0, 65536),
    busy: live.run !== undefined,
  };
}

/** Only our versioned native user entry can carry a separate display form.
 * Legacy, malformed, mixed-role or future records retain their original text;
 * never remove apparent context prefixes from an ordinary user message. */
function nativeUserDisplayText(entry: Record<string, unknown>, models: unknown[]): string | undefined {
  if (entry.kind !== "bk-cat.user" || models.length !== 1 || object(models[0]).role !== "user") return undefined;
  const data = object(entry.data);
  const keys = Object.keys(data);
  if (keys.length !== 2 || !keys.includes("schemaVersion") || !keys.includes("text")
    || data.schemaVersion !== 1 || typeof data.text !== "string"
    || !data.text.trim() || data.text.length > 256 * 1024
    || new TextEncoder().encode(data.text).byteLength > 256 * 1024) return undefined;
  return data.text;
}

function object(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {};
}
function textContent(value: unknown): string {
  if (typeof value === "string") return value;
  if (!Array.isArray(value)) return "";
  return value.map(object).filter((part) => part.type === "text" && typeof part.text === "string")
    .map((part) => part.text as string).join("\n");
}
