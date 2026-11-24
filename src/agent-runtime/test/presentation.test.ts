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

test("stable compaction entry kind has a system display without rewriting its original user model message", () => {
  const original = "The conversation history before this point was compacted into the following summary:\n\n<summary>\nKeep 历史 {1} exactly.\n</summary>";
  const content = Object.freeze([Object.freeze({ type: "text", text: original })]);
  const model = Object.freeze([Object.freeze({ role: "user", content })]);
  const entry = Object.freeze({ kind: "pi.compaction", data: Object.freeze({ reason: "threshold" }), model });
  const entries = Object.freeze([
    entry,
    Object.freeze({ kind: "pi.user", model }),
    Object.freeze({ kind: "pi.compaction.future", model }),
    Object.freeze({ model }),
  ]);
  const before = JSON.stringify(entries);
  assert.deepEqual(conversationPresentation({ entries }).messages, [
    { role: "compaction", text: original },
    { role: "user", text: original },
    { role: "user", text: original },
    { role: "user", text: original },
  ]);
  assert.equal(JSON.stringify(entries), before);
  assert.equal(entry.model[0]!.role, "user");
  assert.equal(entry.model[0]!.content, content);
});

test("compaction display keeps existing native bridge bounds and never guesses from text", () => {
  const original = "The conversation history before this point was compacted into the following summary:" + "字".repeat(40000);
  const entries = Array.from({ length: 6 }, () => ({ kind: "pi.compaction", model: [{ role: "user", content: original }] }));
  const before = JSON.stringify(entries);
  const projection = conversationPresentation({ entries });
  assert.ok(projection.messages.length > 0);
  assert.ok(projection.messages.every((message) => message.role === "compaction" && message.text === original.slice(0, 32768)));
  assert.ok(projection.messages.reduce((total, message) => total + message.text.length, 0) <= 128 * 1024);
  assert.equal(JSON.stringify(entries), before);
  assert.deepEqual(conversationPresentation({ entries: [{ kind: "pi.user", model: [{ role: "user", content: "pi.compaction" }] }] }).messages,
    [{ role: "user", text: "pi.compaction" }]);
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
