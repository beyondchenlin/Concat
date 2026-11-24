// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
import assert from "node:assert/strict";
import test from "node:test";
import { validateNativeModelConfiguration } from "../src/native-model-input.ts";
const configuration={id:"fixture",version:"1",configurationVersion:"1",protocol:"openai-completions",
  endpoint:"https://example.test/v1",model:"fixture-model",requestTimeoutMs:30000,maxOutputTokens:128};
test("model input admits old configurations without loading an original SDK and marks them custom",()=>{
  assert.equal(validateNativeModelConfiguration(configuration).providerId,"custom");
  assert.equal(validateNativeModelConfiguration({...configuration,protocol:"google-generative-ai"}).protocol,"google-generative-ai");
});
test("context capacity requires provenance and native identity fields cannot contain secrets or coerced protocols",()=>{
  for(const change of [{contextWindow:32768},{contextSource:"official-directory:test"},{contextWindow:0,contextSource:"user-conservative:test"},
    {protocol:["openai-completions"]},{providerId:["custom"]},{apiKey:"not-config"},{endpoint:"https://example.test/v1?key=not-config"}]){
    assert.throws(()=>validateNativeModelConfiguration({...configuration,...change}));
  }
  const accepted=validateNativeModelConfiguration({...configuration,contextWindow:32768,contextSource:"user-conservative:fixture-model"});
  assert.equal(accepted.contextWindow,32768);assert.ok(Object.isFrozen(accepted));
});
