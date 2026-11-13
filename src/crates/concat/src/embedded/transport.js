// SPDX-License-Identifier: AGPL-3.0-or-later
(() => {
  "use strict";
  const component = __BKCAT_COMPONENT__;
  const channel = "bk-cat-runtime-v1";
  const maximum = 2 * 1024 * 1024;
  const listeners = new Set();
  const desktop = globalThis.chrome && globalThis.chrome.webview;
  const android = globalThis.BKCatAndroid;
  if ((!desktop || typeof desktop.postMessage !== "function") &&
      (!android || typeof android.postMessage !== "function")) {
    throw new Error("The native runtime transport is unavailable");
  }
  const receive = (serialized) => {
    if (typeof serialized !== "string" || serialized.length > maximum) return;
    let envelope;
    try { envelope = JSON.parse(serialized); } catch { return; }
    if (!envelope || Array.isArray(envelope) || envelope.component !== component ||
        envelope.channel !== channel) return;
    delete envelope.component;
    const event = Object.freeze({ data: JSON.stringify(envelope) });
    for (const listener of Array.from(listeners)) listener(event);
  };
  Object.defineProperty(globalThis, "__BKCatDeliver", {
    configurable: false, writable: false, value: receive,
  });
  if (desktop) desktop.addEventListener("message", (event) => receive(event.data));
  Object.defineProperty(globalThis, "BKCatNative", {
    configurable: false, writable: false,
    value: Object.freeze({
      postMessage(serialized) {
        if (typeof serialized !== "string" || serialized.length > maximum)
          throw new Error("Native frame limit exceeded");
        const envelope = JSON.parse(serialized);
        if (!envelope || Array.isArray(envelope) || envelope.channel !== channel)
          throw new Error("Invalid native runtime frame");
        envelope.component = component;
        const bound = JSON.stringify(envelope);
        if (bound.length > maximum) throw new Error("Native frame limit exceeded");
        if (desktop) desktop.postMessage(bound);
        else android.postMessage(bound);
      },
      addEventListener(type, listener) {
        if (type !== "message" || typeof listener !== "function")
          throw new Error("Unsupported native listener");
        if (listeners.size >= 32) throw new Error("Native listener limit exceeded");
        listeners.add(listener);
      },
      removeEventListener(type, listener) {
        if (type === "message") listeners.delete(listener);
      },
    }),
  });
})();
