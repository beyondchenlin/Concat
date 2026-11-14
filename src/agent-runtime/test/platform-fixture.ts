// SPDX-License-Identifier: AGPL-3.0-or-later

import { webcrypto } from "node:crypto";
import vm from "node:vm";

export interface ScriptFixture {
  src: string;
  onload: () => void;
  onerror: () => void;
}

/** Explicit DOM/native ports for the real transport source; not a WebView. */
export function browserContext(globals: Record<string, unknown> = {}) {
  const scripts: ScriptFixture[] = [];
  const listeners = new Map<string, Set<(event: unknown) => void>>();
  const context = vm.createContext({
    AbortController, AbortSignal, DOMException, TextEncoder, TextDecoder,
    ReadableStream, URL, URLSearchParams, Request, Response, Headers,
    queueMicrotask, structuredClone, crypto: webcrypto, btoa, atob,
    addEventListener(type: string, listener: (event: unknown) => void) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type)!.add(listener);
    },
    removeEventListener(type: string, listener: (event: unknown) => void) {
      listeners.get(type)?.delete(listener);
    },
    document: {
      createElement() { return {}; },
      head: { appendChild(script: ScriptFixture) { scripts.push(script); } },
    },
    ...globals,
  });
  context.window = context;
  const emit = (type: string, event: unknown = {}) => {
    for (const listener of [...(listeners.get(type) ?? [])]) listener(event);
  };
  return { context, scripts, emit };
}
