// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
// Executes the actual pinned adapters. Only network is intercepted; no provider
// account, real key or model inference is used by these transport-shape tests.
import assert from "node:assert/strict";
import test from "node:test";
import vm from "node:vm";
import { webcrypto } from "node:crypto";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { packageRoot, referenceRoot, referencePaths, verifyReference } from "../scripts/reference.mjs";

const require = verifyReference();
const { build } = await import(pathToFileURL(require.resolve("esbuild")).href);
const output = await build({
  absWorkingDir: referenceRoot,
  stdin: { contents: [
    `export { nativeModelConfiguration } from ${JSON.stringify(resolve(packageRoot,"src/model-configuration.ts"))};`,
    `export { nativeModels } from ${JSON.stringify(resolve(packageRoot,"src/models.ts"))};`,
    `export { ExecutionGate } from ${JSON.stringify(resolve(packageRoot,"src/gate.ts"))};`,
    `export { normalizeContext } from "@earendil-works/pi-ai/utils/transcript";`,
  ].join("\n"), loader: "ts", resolveDir: packageRoot },
  tsconfigRaw: { compilerOptions: { paths: referencePaths() } }, nodePaths: [resolve(referenceRoot,"node_modules")],
  bundle: true, platform: "browser", target: "es2022", format: "iife", globalName: "ModelServices", write: false,
});
let ambientRequests = 0;
const sandbox = vm.createContext({
  AbortController, AbortSignal, DOMException, URL, URLSearchParams, Request, Response, Headers, FormData, Blob, File,
  ReadableStream, TextDecoder, TextEncoder, Uint8Array, setTimeout, clearTimeout, queueMicrotask, performance,
  crypto: webcrypto, btoa, atob, structuredClone,
  fetch() { ambientRequests++; throw new Error("Ambient network is not allowed"); },
});
vm.runInContext(output.outputFiles[0].text,sandbox);
const api=sandbox.ModelServices;
const nativeJson=vm.runInContext("JSON.parse",sandbox);
const raw=(protocol="openai-completions", override={}) => ({
  id:"isolated-connection", version:"1", configurationVersion:"1", protocol, providerId:"custom",
  endpoint:"https://model.invalid/custom",model:"explicit-model",requestTimeoutMs:30000,maxOutputTokens:128,
  contextWindow:8192,contextSource:"user-conservative:explicit-model",...override,
});
function fixture(configuration) {
  const lease={storageId:"models",generation:"1",handle:"model-owner"};const gate=new api.ExecutionGate();
  gate.acquiring();gate.owned(lease);gate.activating(lease,{generation:"1",snapshotId:"reviewed",allowInitializeEmptyStore:false});gate.ready(lease);
  gate.authorizeNetwork(lease,"only-this-native-task");const requests=[];
  const models=api.nativeModels(configuration,{network:{async fetch(_lease,permit,input,init){
    assert.equal(permit,"only-this-native-task");const request=new Request(input,init);
    requests.push({url:request.url,method:request.method,headers:[...request.headers],body:JSON.parse(await request.text())});
    throw new Error("Deterministic request capture; no real service call");
  }}},gate,lease);
  return {models,gate,requests};
}
const context=()=>nativeJson(JSON.stringify({messages:[{role:"user",content:"Isolated adapter check",timestamp:1}],
  systemPrompt:"Use the supplied native tools only.",tools:[{name:"project_get",description:"Read the current project",parameters:{type:"object",properties:{}}}]}));

test("all four pinned original adapters keep the selected protocol, native transport and output ceiling",{timeout:20000},async()=>{
  for(const protocol of ["openai-completions","openai-responses","anthropic-messages","google-generative-ai"]){
    const configured=api.nativeModelConfiguration(raw(protocol));const f=fixture(configured);
    const model=f.models.getAllModels()[0];
    const result=await f.models.streamSimple(model,context(),{maxTokens:99999}).result();
    assert.equal(result.stopReason,"error");assert.equal(f.requests.length,1,`${protocol}: one intercepted request, no retry`);
    const request=f.requests[0];assert.equal(request.method,"POST");
    if(protocol==="google-generative-ai"){
      assert.equal(request.url,"https://model.invalid/custom/models/explicit-model:streamGenerateContent?alt=sse");
      assert.equal(request.body.generationConfig.maxOutputTokens,128);
      assert.ok(Array.isArray(request.body.contents));assert.equal(request.body.tools[0].functionDeclarations[0].name,"project_get");
      assert.equal(request.body.model,undefined);assert.equal(request.body.messages,undefined);
      const allowed=new Set(["contents","systemInstruction","generationConfig","tools","toolConfig"]);
      assert.deepEqual(Object.keys(request.body).filter(key=>!allowed.has(key)),[]);
    }else{
      assert.equal(request.body.model,"explicit-model");assert.equal(request.body.stream,true);
      assert.equal(request.body.max_tokens??request.body.max_completion_tokens??request.body.max_output_tokens,128);
    }
    assert.equal(JSON.stringify(request).includes("user-conservative"),false,"capacity provenance is local configuration, not prompt content");
    f.gate.stop();
  }
  assert.equal(ambientRequests,0);
});

test("fixed original capacity requires exact official provider, address, protocol and model",()=>{
  const official=raw("openai-completions",{providerId:"deepseek",endpoint:"https://api.deepseek.com/",model:"deepseek-flash",contextWindow:null,contextSource:null});
  const configuration=api.nativeModelConfiguration(official);
  assert.equal(configuration.contextKnown,true);assert.equal(configuration.native.contextWindow,1000000);
  assert.match(configuration.contextSource,/^locked-pi-1\.5\.2:d36266e/);
  for(const override of [{providerId:"custom"},{endpoint:"https://proxy.invalid"},{endpoint:"https://api.deepseek.com/not-the-service-base"},
    {protocol:"openai-responses"},{model:"a-similar-model"}]){
    const unknown=api.nativeModelConfiguration({...official,...override});assert.equal(unknown.contextKnown,false);
    assert.equal(unknown.providers[0].getModels()[0].contextWindow,0);
  }
  const fresh=api.nativeModelConfiguration({...official,contextWindow:1048576,contextSource:"official-directory:isolated-test"});
  assert.equal(fresh.native.contextWindow,1048576);assert.equal(fresh.contextSource,"official-directory:isolated-test");
  assert.equal(ambientRequests,0);
});

test("a stable native models handle swaps original registries for the next task and retains captured provider limits",{timeout:20000},async()=>{
  let current=api.nativeModelConfiguration(raw());const f=fixture(()=>current);
  const oldModel=f.models.getAllModels()[0];const oldProvider=f.models.getProvider(current.native.id);
  await f.models.streamSimple(oldModel,context(),{}).result();
  current=api.nativeModelConfiguration(raw("openai-completions",{id:"second-connection",model:"second-model",maxOutputTokens:64,endpoint:"https://second.invalid/v1"}));
  const next=f.models.getAllModels();assert.equal(next.length,1);assert.equal(next[0].id,"second-model");
  assert.equal(f.models.getProvider("isolated-connection"),undefined);
  await f.models.streamSimple(next[0],context(),{}).result();
  await oldProvider.streamSimple(oldModel,api.normalizeContext(context()),{maxTokens:9999,apiKey:"sk-isolated-native-placeholder"}).result();
  assert.deepEqual(f.requests.map(request=>[request.url,request.body.model,request.body.max_tokens??request.body.max_completion_tokens]),[
    ["https://model.invalid/custom/chat/completions","explicit-model",128],
    ["https://second.invalid/v1/chat/completions","second-model",64],
    ["https://model.invalid/custom/chat/completions","explicit-model",128],
  ]);
  f.gate.stop();assert.equal(ambientRequests,0);
});

test("DeepSeek reasoning replay capability is limited to exact official chat bases and retains legacy identity",()=>{
  for(const endpoint of ["https://api.deepseek.com","https://api.deepseek.com/","https://api.deepseek.com/v1","https://api.deepseek.com/v1/"]){
    for(const modelId of ["deepseek-flash","an-explicit-future-model"]){
      const input=raw("openai-completions",{providerId:"custom",endpoint,model:modelId});
      const before=JSON.stringify(input);const configuration=api.nativeModelConfiguration(input);const model=configuration.providers[0].getModels()[0];
      assert.equal(model.reasoning,true);assert.equal(model.provider,input.id);assert.equal(configuration.native.providerId,"custom");
      assert.equal(model.compat.supportsReasoningEffort,false);assert.equal(model.compat.thinkingFormat,"openai");
      assert.equal(JSON.stringify(input),before,"registration does not migrate connection or credential scope");
    }
  }
  for(const endpoint of ["https://proxy.example.test","https://api.deepseek.com.evil.example","https://api.deepseek.com/custom","https://api.deepseek.com:8443","http://127.0.0.1","https://api.openai.com/v1"]){
    const model=api.nativeModelConfiguration(raw("openai-completions",{endpoint,model:"deepseek-flash",providerId:"deepseek"})).providers[0].getModels()[0];
    assert.equal(model.reasoning,false,"model name or provider label cannot select official endpoint compatibility");
  }
  for(const protocol of ["openai-responses","anthropic-messages","google-generative-ai"]){
    const model=api.nativeModelConfiguration(raw(protocol,{endpoint:"https://api.deepseek.com",model:"deepseek-flash",providerId:"custom"})).providers[0].getModels()[0];
    assert.equal(model.reasoning,false,"other protocol registrations keep their previous capability");
  }
  assert.equal(ambientRequests,0);
});

function replayFixture(configuration,thought){
  const lease={storageId:"replay",generation:"1",handle:"model-owner"};const gate=new api.ExecutionGate();
  gate.acquiring();gate.owned(lease);gate.activating(lease,{generation:"1",snapshotId:"reviewed",allowInitializeEmptyStore:false});gate.ready(lease);
  gate.authorizeNetwork(lease,"only-this-native-task");const requests=[];
  const models=api.nativeModels(configuration,{network:{async fetch(_lease,permit,input,init){
    assert.equal(permit,"only-this-native-task");const request=new Request(input,init);const body=JSON.parse(await request.text());requests.push(body);
    const first=requests.length===1;
    const delta=first?{role:"assistant",reasoning_content:thought,tool_calls:[{index:0,id:"fixture-call",type:"function",function:{name:"project_get",arguments:"{}"}}]}:{role:"assistant",content:"Explicit fixture completion."};
    const chunk={id:"replay-fixture",object:"chat.completion.chunk",created:1,model:body.model,choices:[{index:0,delta,finish_reason:first?"tool_calls":"stop"}]};
    return new Response(`data: ${JSON.stringify(chunk)}\n\ndata: [DONE]\n\n`,{status:200,headers:{"content-type":"text/event-stream"}});
  }}},gate,lease);
  return {models,gate,requests};
}

test("production registration and original adapter replay nonempty, empty and whitespace thinking without changing request mode",{timeout:20000},async()=>{
  for(const [name,thought,previousModel,expected] of [
    ["nonempty","Explicit fixture reasoning.",undefined,"Explicit fixture reasoning."],
    ["empty","",undefined,""],
    ["whitespace"," ",undefined,""],
    ["other model history","Private fixture reasoning from another model.","previous-model",""],
  ]){
    const configuration=api.nativeModelConfiguration(raw("openai-completions",{providerId:"custom",endpoint:"https://api.deepseek.com/",model:"deepseek-flash"}));
    const f=replayFixture(configuration,thought);const model=f.models.getAllModels()[0];
    try{
      const initial=context();const first=await f.models.streamSimple(model,initial,{}).result();
      assert.notEqual(first.stopReason,"error",`${name}: the original adapter parsed the fixture`);
      assert.equal(first.content.filter(part=>part.type==="toolCall").length,1);
      const historical=previousModel?{...first,model:previousModel}:first;
      const next=nativeJson(JSON.stringify({...initial,messages:[...initial.messages,historical,{role:"toolResult",toolCallId:"fixture-call",toolName:"project_get",content:[{type:"text",text:"Explicit fixture result; no project read occurred."}],isError:false,timestamp:2}]}));
      const second=await f.models.streamSimple(model,next,{}).result();assert.notEqual(second.stopReason,"error");assert.equal(f.requests.length,2);
      const replay=f.requests[1].messages.find(message=>message.role==="assistant"&&message.tool_calls?.length);
      assert.ok(replay);assert.equal(Object.hasOwn(replay,"reasoning_content"),true,`${name}: the original replay field is present`);
      assert.equal(replay.reasoning_content,expected);
      if(previousModel)assert.equal(JSON.stringify(f.requests[1]).includes(thought),false,"foreign model reasoning is never replayed as content");
      for(const request of f.requests){
        assert.equal(request.max_tokens,128,"the native per-request output ceiling is preserved");
        assert.equal(Object.hasOwn(request,"thinking"),false);assert.equal(Object.hasOwn(request,"reasoning_effort"),false);
        assert.equal(request.model,"deepseek-flash");assert.equal(request.tools[0].function.name,"project_get");
      }
    }finally{f.gate.stop();}
  }
  assert.equal(ambientRequests,0,"only explicit fixture transport ran");
});
