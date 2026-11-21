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

test("versioned native user metadata displays the captured draft without changing model context", () => {
  const full = "原生当前工程上下文：{\"projectId\":\"isolated-project\",\"expectedRevision\":3}\n任务权限：只读。\n用户要求：保留整条模型输入";
  const draft = "  原生当前工程上下文：这也是我的原文\n用户要求：不要按这些词裁剪。  ";
  const entry = { kind: "bk-cat.user", data: { schemaVersion: 1, text: draft }, model: [{ role: "user", content: full }] };
  const before = JSON.stringify(entry);
  assert.deepEqual(conversationPresentation({ entries: [entry] }).messages, [{ role: "user", text: draft }]);
  assert.equal(JSON.stringify(entry), before);
  assert.equal(entry.model[0]!.content, full);
});

test("legacy and invalid display metadata fall back to original content without prefix guessing", () => {
  const full = "原生当前工程上下文：{\"path\":\"isolated-project\"}\n任务权限：只读。\n用户要求：这部分也必须保留";
  const models = [{ role: "user", content: full }];
  const invalid = [
    {}, { kind: "pi.user", data: { schemaVersion: 1, text: "incorrect override" } },
    { kind: "bk-cat.user" },
    ...[null, [], "text", {}, { schemaVersion: 2, text: "future" }, { schemaVersion: "1", text: "wrong version type" },
      { schemaVersion: 1, text: "" }, { schemaVersion: 1, text: " \n\t" }, { schemaVersion: 1, text: 9 },
      { schemaVersion: 1, text: "too many fields", authority: "edit" },
      { schemaVersion: 1, text: "字".repeat(256 * 1024 / 3 + 1) },
    ].map((data) => ({ kind: "bk-cat.user", data })),
  ];
  for (const metadata of invalid) {
    assert.deepEqual(conversationPresentation({ entries: [{ ...metadata, model: models }] }).messages,
      [{ role: "user", text: full }]);
  }
});

test("display metadata never replaces assistant, tool or mixed-entry content", () => {
  const metadata = { kind: "bk-cat.user", data: { schemaVersion: 1, text: "must not replace other messages" } };
  const messages = [
    { role: "user", content: "mixed user" },
    { role: "assistant", content: [{ type: "text", text: "real assistant" }] },
    { role: "toolResult", content: [{ type: "text", text: "real tool" }] },
  ];
  assert.deepEqual(conversationPresentation({ entries: [{ ...metadata, model: messages }] }).messages, [
    { role: "user", text: "mixed user" }, { role: "assistant", text: "real assistant" }, { role: "tool", text: "real tool" },
  ]);
  for (const message of messages.slice(1)) {
    assert.equal(conversationPresentation({ entries: [{ ...metadata, model: [message] }] }).messages[0]?.text,
      message.role === "assistant" ? "real assistant" : "real tool");
  }
});

test("display window stays bounded and preserves newest messages without changing the original records", () => {
  const entries = Array.from({ length: 200 }, (_, index) => ({ model: [{ role: "user", content: `${index}:` + "字".repeat(32768) }] }));
  const projection = conversationPresentation({ entries });
  assert.ok(projection.messages.at(-1)?.text.startsWith("199:"));
  assert.ok(projection.messages.reduce((total, item) => total + item.text.length, 0) <= 128 * 1024);
  assert.equal(entries.length, 200);
  assert.equal(entries[199]!.model[0]!.content.length, 32772);
});
