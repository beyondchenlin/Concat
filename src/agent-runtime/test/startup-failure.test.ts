// SPDX-License-Identifier: AGPL-3.0-or-later

import test from "node:test";
import assert from "node:assert/strict";
import { safeStartupFailure } from "../src/browser-entry.ts";

test("startup diagnostics redact local paths, URLs and credentials", () => {
  const message = safeStartupFailure(new Error(
    "failed C:\\Users\\ai\\hidden.db https://example.test/private?key=value Bearer abcdef sk-secretvalue123 api_key=anothersecret",
  ));
  assert(!message.includes("Users"));
  assert(!message.includes("example.test"));
  assert(!message.includes("abcdef"));
  assert(!message.includes("secretvalue123"));
  assert(!message.includes("anothersecret"));
  assert(message.includes("[本地路径]"));
  assert(message.includes("[网络地址]"));
});

test("startup diagnostics remain single-line and bounded", () => {
  assert.equal(safeStartupFailure({ secret: "opaque" }), "运行组件初始化失败");
  assert.equal(safeStartupFailure(new Error("x".repeat(5000) + "\nsecret")).length, 384);
  assert(!safeStartupFailure("first\nsecond").includes("\n"));
});
