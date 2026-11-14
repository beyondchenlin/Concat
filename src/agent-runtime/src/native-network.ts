// SPDX-License-Identifier: AGPL-3.0-or-later

import { RuntimeRefused } from "./gate.ts";
import type { NativeWireClient } from "./native-wire.ts";
import type { OwnerLease } from "./ports.ts";

const MAX_BODY_BYTES = 1024 * 1024;
const MAX_RESPONSE_BYTES = 32 * 1024 * 1024;
const MAX_CHUNK_BYTES = 64 * 1024;
const REQUEST_HEADERS = new Set(["accept", "content-type", "anthropic-version", "anthropic-beta"]);
const RESPONSE_HEADERS = new Set(["content-type"]);

/** A pull-based native stream gives the original SDK Fetch semantics without
 * putting a secret, cookie jar, redirect policy, or browser socket in the page. */
export async function nativeNetworkFetch(
  wire: NativeWireClient, lease: OwnerLease, networkPermit: string, input: RequestInfo | URL, init: RequestInit,
): Promise<Response> {
  wire.assertOwner(lease);
  if (!networkPermit || networkPermit.length > 256) throw new RuntimeRefused("native network permission is missing");
  const request = new Request(input, init);
  request.signal.throwIfAborted();
  if (request.method !== "POST") throw new RuntimeRefused("model transport permits only reviewed POST requests");
  const body = await request.text();
  if (new TextEncoder().encode(body).byteLength > MAX_BODY_BYTES) throw new RuntimeRefused("model request exceeds the native message limit");
  request.signal.throwIfAborted();
  wire.assertOwner(lease);
  const headers = [...request.headers].filter(([name]) => REQUEST_HEADERS.has(name));
  const owner = { ownerHandle: lease.handle, generation: lease.generation };
  const open = {
    method: "network.open", ...owner, networkPermit, url: request.url, methodName: request.method, headers, body,
  };
  if (new TextEncoder().encode(JSON.stringify(open)).byteLength > 1536 * 1024) {
    throw new RuntimeRefused("serialized model request exceeds the native message limit");
  }
  const opened = object(await wire.call(open, request.signal));
  if (typeof opened.streamId !== "string" || !opened.streamId || opened.streamId.length > 256) {
    throw new RuntimeRefused("native network open did not return a stream");
  }
  const streamId = opened.streamId;
  let terminal = false;
  let controller: ReadableStreamDefaultController<Uint8Array> | undefined;
  let received = 0;
  let detachRevoked = () => {};
  let detachClosed = () => {};
  const clean = () => {
    request.signal.removeEventListener("abort", aborted);
    detachRevoked(); detachClosed();
  };
  const cancel = () => wire.call({ method: "network.cancel", ...owner, networkPermit, streamId }).then(() => {}).catch(() => {});
  const endWithError = (message: string) => {
    if (terminal) return;
    terminal = true; clean();
    controller?.error(new RuntimeRefused(message));
    void cancel();
  };
  const aborted = () => endWithError("model request was cancelled");
  try {
    request.signal.throwIfAborted();
    wire.assertOwner(lease);
    if (!Number.isInteger(opened.status) || Number(opened.status) < 200 || Number(opened.status) > 299
      || [204, 205].includes(Number(opened.status))) throw new RuntimeRefused("model service returned an unsupported response");
    const responseHeaders = new Headers();
    if (!Array.isArray(opened.headers) || opened.headers.length > 64) throw new RuntimeRefused("native response headers are invalid");
    for (const pair of opened.headers) {
      if (!Array.isArray(pair) || pair.length !== 2 || typeof pair[0] !== "string" || typeof pair[1] !== "string") {
        throw new RuntimeRefused("native response headers are invalid");
      }
      if (RESPONSE_HEADERS.has(pair[0].toLowerCase())) responseHeaders.set(pair[0], pair[1]);
    }
    const stream = new ReadableStream<Uint8Array>({
      start(value) { controller = value; },
      async pull(value) {
        if (terminal) return;
        try {
          request.signal.throwIfAborted(); wire.assertOwner(lease);
          const next = object(await wire.call({ method: "network.read", ...owner, networkPermit, streamId }, request.signal));
          if (terminal) return;
          request.signal.throwIfAborted(); wire.assertOwner(lease);
          if (typeof next.done !== "boolean" || typeof next.chunk !== "string" || next.chunk.length > 87384
            || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(next.chunk)) {
            throw new RuntimeRefused("native model stream chunk is invalid");
          }
          const decoded = atob(next.chunk);
          if (decoded.length > MAX_CHUNK_BYTES || (received += decoded.length) > MAX_RESPONSE_BYTES
            || (!next.done && decoded.length === 0)) throw new RuntimeRefused("native model stream exceeds its bounds");
          if (decoded.length) value.enqueue(Uint8Array.from(decoded, (character) => character.charCodeAt(0)));
          if (next.done) { terminal = true; clean(); value.close(); }
        } catch { endWithError("model stream failed or was cancelled"); }
      },
      async cancel() { if (terminal) return; terminal = true; clean(); await cancel(); },
    });
    request.signal.addEventListener("abort", aborted, { once: true });
    detachRevoked = wire.onRevoked(lease, aborted);
    detachClosed = wire.onClosed(aborted);
    if (request.signal.aborted) aborted();
    wire.assertOwner(lease);
    return new Response(stream, { status: Number(opened.status), headers: responseHeaders });
  } catch (error) {
    terminal = true; clean(); await cancel();
    throw error;
  }
}

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new RuntimeRefused("native model response is invalid");
  return value as Record<string, unknown>;
}
