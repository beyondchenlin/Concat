// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { conversationPresentation } from "../src/presentation.ts";

test("native presentation projects real transcript and streamed text without thought blocks or fabricated status", () => {
  assert.deepEqual(conversationPresentation({
    entries: [
      { model: [{ role: "user", content: "我的文案" }] },
      { model: [{ role: "assistant", content: [{ type: "thinking", thinking: "hidden" }, { type: "text", text: "已有回答" }] }] },
      { model: [{ role: "toolResult", content: [{ type: "text", text: "实际工具结果" }] }] },
    ],
    docs: { "pi.live": { run: { taskId: 2 }, generation: { message: { content: [{ type: "text", text: "正在输出" }] } } }, "pi.usage": { total: 123 } },
  }), {
    messages: [{ role: "user", text: "我的文案" }, { role: "assistant", text: "已有回答" }, { role: "tool", text: "实际工具结果" }],
    partialText: "正在输出", busy: true,
  });
  assert.deepEqual(conversationPresentation({ entries: [], docs: { "pi.live": { error: "private-provider-error" } } }), {
    messages: [], partialText: "", busy: false,
  });
});

test("display window stays bounded and preserves newest messages without changing the original records", () => {
  const entries = Array.from({ length: 200 }, (_, index) => ({ model: [{ role: "user", content: `${index}:` + "字".repeat(32768) }] }));
  const projection = conversationPresentation({ entries });
  assert.ok(projection.messages.at(-1)?.text.startsWith("199:"));
  assert.ok(projection.messages.reduce((total, item) => total + item.text.length, 0) <= 128 * 1024);
  assert.equal(entries.length, 200);
  assert.equal(entries[199]!.model[0]!.content.length, 32772);
});
