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
    const models = object(entry).model;
    if (!Array.isArray(models)) continue;
    for (const message of models.slice().reverse()) {
      const item = object(message);
      const role = item.role === "toolResult" ? "tool" : item.role;
      if (role !== "user" && role !== "assistant" && role !== "tool") continue;
      const text = textContent(item.content).slice(0, Math.min(32768, remaining));
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

function object(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {};
}
function textContent(value: unknown): string {
  if (typeof value === "string") return value;
  if (!Array.isArray(value)) return "";
  return value.map(object).filter((part) => part.type === "text" && typeof part.text === "string")
    .map((part) => part.text as string).join("\n");
}
