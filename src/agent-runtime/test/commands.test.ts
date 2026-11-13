// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { conversationOptions, submissionDraft } from "../src/commands.ts";

test("conversation creation cannot install executable callbacks, task ownership, or registry objects", () => {
  assert.deepEqual(conversationOptions({}), { ownership: { kind: "ownerless" } });
  assert.deepEqual(conversationOptions({ agent: { model: { provider: "chosen", modelId: "chosen-model" }, maxTurns: 4 } }), {
    ownership: { kind: "ownerless" }, agent: { model: { provider: "chosen", modelId: "chosen-model" }, maxTurns: 4 },
  });
  for (const input of [{ init: () => {} }, { ownership: { kind: "task", taskId: 1 } }, { agent: { tools: [] } }, { agent: { model: { provider: "", modelId: "x" } } }, { agent: { maxTurns: 0 } }]) {
    assert.throws(() => conversationOptions(input));
  }
});

test("chat submission requires native deduplication identity and cannot forge transcript entries or queued follow-ups", () => {
  assert.deepEqual(submissionDraft({ type: "input", requestId: "native-1", content: "Add two captions" }), {
    type: "input", requestId: "native-1", content: "Add two captions", whenBusy: "reject",
  });
  assert.deepEqual(submissionDraft({ type: "continue", requestId: "native-2" }), { type: "continue", requestId: "native-2" });
  for (const input of [{ type: "input", content: "x" }, { type: "write", requestId: "x", entry: { kind: "pi.assistant" } }, { type: "input", requestId: "x", content: " " }, { type: "input", requestId: "x", content: "x", whenBusy: "followUp" }, { type: "continue", requestId: "x", content: "x" }]) {
    assert.throws(() => submissionDraft(input));
  }
});
