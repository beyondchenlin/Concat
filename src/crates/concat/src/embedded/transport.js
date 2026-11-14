// SPDX-License-Identifier: AGPL-3.0-or-later

// Keep this admission/diagnostic script in ES5 syntax. An older engine must be
// able to report missing capabilities before parsing the fixed original bundle.
// The native owner independently enforces a monotonic loading deadline if even
// this script cannot run. No browser API is replaced and no ready event is made
// up here: only the actual bootstrap may announce runtime.loaded.
(function (host) {
  "use strict";
  var component = __BKCAT_COMPONENT__;
  var channel = "bk-cat-runtime-v1";
  var maximum = 2 * 1024 * 1024;
  var desktop = host.chrome && host.chrome.webview;
  var android = host.BKCatAndroid;
  var retired = false;
  var loaded = false;
  var listeners;

  function send(bound) {
    if (desktop && typeof desktop.postMessage === "function") desktop.postMessage(bound);
    else if (android && typeof android.postMessage === "function") android.postMessage(bound);
    else throw new Error("The native runtime transport is unavailable");
  }

  function removeStartupListeners() {
    host.removeEventListener("error", scriptError);
    host.removeEventListener("unhandledrejection", scriptError);
  }

  function retire() {
    retired = true;
    if (listeners) listeners.clear();
    removeStartupListeners();
  }

  function fail(code) {
    if (retired || loaded) return;
    retire();
    // Only fixed codes cross this path. Never serialize an exception, console
    // body, resource URL, user agent, conversation, credential or database data.
    try {
      send(JSON.stringify({ channel: channel, component: component,
        event: "runtime.loadFailed", code: code }));
    } catch (_) {
      // Native's independent deadline closes a silent or unavailable bridge.
    }
  }

  function scriptError() { fail("script-execution"); }

  function callable(path) {
    var value = host;
    var parts = path.split(".");
    for (var index = 0; index < parts.length; index++) {
      if (value === null || value === undefined) return false;
      value = value[parts[index]];
    }
    return typeof value === "function";
  }

  // This is a capability contract for the pinned bundle, not a minimum browser
  // version claim. The original schema cache needs native weak-reference/GC
  // semantics; durable migrations use .at(); chord uses Object.hasOwn() and
  // promise resolvers; our native wire and execution gate need UUID/cancellation.
  // The original model/MCP components also use clone, stream and web data APIs.
  var requirements = [
    ["web-platform", ["Set", "Map", "WeakMap", "Symbol", "Proxy", "Promise",
      "Object.freeze", "Object.defineProperty", "Array.from", "queueMicrotask",
      "TextEncoder", "TextDecoder", "ReadableStream", "URL", "URLSearchParams",
      "Request", "Response", "Headers", "DOMException", "btoa", "atob"]],
    ["weak-references", ["WeakRef", "WeakRef.prototype.deref", "FinalizationRegistry",
      "FinalizationRegistry.prototype.register"]],
    ["secure-random", ["crypto.getRandomValues", "crypto.randomUUID"]],
    ["cancellation", ["AbortController", "AbortSignal", "AbortSignal.any",
      "AbortSignal.prototype.throwIfAborted"]],
    ["collection-methods", ["Array.prototype.at", "Object.hasOwn"]],
    ["promise-resolvers", ["Promise.withResolvers"]],
    ["structured-clone", ["structuredClone"]]
  ];

  host.addEventListener("error", scriptError);
  host.addEventListener("unhandledrejection", scriptError);
  host.addEventListener("pagehide", retire);
  if ((!desktop || typeof desktop.postMessage !== "function") &&
      (!android || typeof android.postMessage !== "function")) {
    throw new Error("The native runtime transport is unavailable");
  }
  for (var group = 0; group < requirements.length; group++) {
    for (var method = 0; method < requirements[group][1].length; method++) {
      if (!callable(requirements[group][1][method])) {
        fail(requirements[group][0]);
        return;
      }
    }
  }
  if (!host.globalThis || !("reason" in host.AbortSignal.prototype)) {
    fail("web-platform");
    return;
  }

  listeners = new Set();
  function receive(serialized) {
    if (retired || typeof serialized !== "string" || serialized.length > maximum) return;
    var envelope;
    try { envelope = JSON.parse(serialized); } catch (_) { return; }
    if (!envelope || Array.isArray(envelope) || envelope.component !== component ||
        envelope.channel !== channel) return;
    delete envelope.component;
    var event = Object.freeze({ data: JSON.stringify(envelope) });
    var snapshot = Array.from(listeners);
    for (var index = 0; index < snapshot.length; index++) snapshot[index](event);
  }
  Object.defineProperty(host, "__BKCatDeliver", {
    configurable: false, writable: false, value: receive
  });
  if (desktop) desktop.addEventListener("message", function (event) { receive(event.data); });
  Object.defineProperty(host, "BKCatNative", {
    configurable: false, writable: false,
    value: Object.freeze({
      postMessage: function (serialized) {
        if (retired) throw new Error("Native runtime transport has retired");
        if (typeof serialized !== "string" || serialized.length > maximum)
          throw new Error("Native frame limit exceeded");
        var envelope = JSON.parse(serialized);
        if (!envelope || Array.isArray(envelope) || envelope.channel !== channel)
          throw new Error("Invalid native runtime frame");
        envelope.component = component;
        var bound = JSON.stringify(envelope);
        if (bound.length > maximum) throw new Error("Native frame limit exceeded");
        send(bound);
        if (envelope.event === "runtime.loaded") {
          loaded = true;
          removeStartupListeners();
        }
      },
      addEventListener: function (type, listener) {
        if (retired) throw new Error("Native runtime transport has retired");
        if (type !== "message" || typeof listener !== "function")
          throw new Error("Unsupported native listener");
        if (listeners.size >= 32) throw new Error("Native listener limit exceeded");
        listeners.add(listener);
      },
      removeEventListener: function (type, listener) {
        if (type === "message") listeners.delete(listener);
      }
    })
  });

  function load(path, complete) {
    if (retired) return;
    var script = host.document.createElement("script");
    var completed = false;
    script.src = path;
    script.onload = function () {
      if (retired || completed) return;
      completed = true;
      complete();
    };
    script.onerror = function () {
      if (completed) return;
      completed = true;
      fail("script-resource");
    };
    host.document.head.appendChild(script);
  }
  load("/runtime.js", function () {
    if (!host.BKCatRuntime) { fail("runtime-missing"); return; }
    load("/bootstrap.js", function () {
      if (!loaded) fail("bootstrap-missing");
    });
  });
})(window);
