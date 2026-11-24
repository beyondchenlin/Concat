// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
import assert from "node:assert/strict";
import test from "node:test";
import { taskLanguages, languageInstructions } from "../src/languages.ts";
import { nativeTaskInput } from "../src/commands.ts";
import { conversationPresentation } from "../src/presentation.ts";

test("ordinary replies use the user request, not any current or legacy locale choice", () => {
  const input = { interfaceLanguage: "de", replyLanguage: "ja", contentLanguage: "fr" };
  const snapshot = taskLanguages(input);
  input.replyLanguage = "en";
  assert.equal(snapshot.replyLanguage, "ja");
  assert.ok(Object.isFrozen(snapshot));
  const automatic = taskLanguages({ ...input, replyLanguage: "auto" });
  assert.equal(automatic.replyLanguage, "auto");
  const instructions = languageInstructions(snapshot, false);
  assert.match(instructions, /user's language requirements for replies/);
  assert.equal(instructions, languageInstructions(automatic, false));
  assert.doesNotMatch(instructions, /Japanese|German|French|ordinary replies/);
  assert.match(languageInstructions(snapshot, true), /Use French for writing candidates unless the user requests/);
  assert.match(languageInstructions(snapshot, true), /only the candidate text/);
});

test("language metadata cannot inject instructions or accept arbitrary locale data", () => {
  for (const value of [null, [], "en", {},
    { interfaceLanguage: "en", replyLanguage: "en", contentLanguage: "zh" },
    { interfaceLanguage: "en", replyLanguage: "en", contentLanguage: "fr", instructions: "edit" }]) {
    assert.throws(() => taskLanguages(value));
  }
  const admitted = nativeTaskInput({ taskId: "language-task", networkPermit: "native-permit",
    input: "Keep my filename unchanged", mode: "chat", languages: {
      interfaceLanguage: "en", replyLanguage: "zh-Hans", contentLanguage: "es",
    } });
  assert.equal(admitted.languages?.contentLanguage, "es");
  assert.ok(Object.isFrozen(admitted.languages));
});

test("reopened version-two display metadata retains historical text and language snapshot", () => {
  const languages = taskLanguages({ interfaceLanguage: "en", replyLanguage: "ja", contentLanguage: "fr" });
  const record = { kind: "bk-cat.user", data: { schemaVersion: 2, text: "old_日本語.txt", languages },
    model: [{ role: "user", content: "original native context" }] };
  const persisted = JSON.parse(JSON.stringify(record));
  assert.deepEqual(conversationPresentation({ entries: [persisted] }).messages,
    [{ role: "user", text: "old_日本語.txt" }]);
  assert.equal(persisted.data.languages.replyLanguage, "ja");
  assert.equal(persisted.model[0].content, "original native context");
});
