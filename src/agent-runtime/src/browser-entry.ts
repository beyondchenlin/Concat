// SPDX-License-Identifier: AGPL-3.0-or-later

import type { NativeMessageBridge } from "./native-wire.ts";
import type { AgentRuntime } from "./runtime.ts";
import type { ConversationObservation, RecoveryApproval, SchedulingAction } from "./ports.ts";

type RuntimeLibrary = typeof import("./index.ts");
type HostWindow = Window & typeof globalThis & { BKCatNative?: NativeMessageBridge; BKCatRuntime?: RuntimeLibrary };

/** One controlled component. Native commands come from the native UI, not
 * from model tools. The component must be replaced to start a new host. */
export function attachNativeController(library: RuntimeLibrary, bridge: NativeMessageBridge): () => void {
  const wire = new library.NativeWireClient(bridge);
  const ports = library.messageNativePorts(wire);
  let runtime: AgentRuntime | undefined;
  let observation: ConversationObservation | undefined;
  let observationEpoch = 0;
  let disposed = false;
  const accepted = new Set<string>();
  const reply = (id: string, result: unknown, error?: unknown) => {
    if (disposed) return;
    bridge.postMessage(JSON.stringify({ channel: library.NATIVE_CHANNEL, id,
      ...(error === undefined ? { result: result ?? null } : { error: { code: "refused", message: error instanceof Error ? error.message : String(error) } }),
    }));
  };
  const phase = () => {
    if (!disposed) bridge.postMessage(JSON.stringify({ channel: library.NATIVE_CHANNEL, event: "runtime.phase", phase: runtime?.phase ?? "new" }));
  };
  const requireRuntime = () => {
    if (!runtime) throw new library.RuntimeRefused("native runtime has not started");
    return runtime;
  };
  const command = async (payload: Record<string, unknown>): Promise<unknown> => {
    switch (payload.method) {
      case "runtime.start": {
        if (runtime) throw new library.RuntimeRefused("a component can start only one host generation");
        if (typeof payload.storageId !== "string" || !payload.storageId) throw new library.RuntimeRefused("native storage identity is required");
        // Missing channel/settings are explicit. Do not search another
        // project's credentials or import a generated placeholder catalog.
        const denied = async () => { throw new library.RuntimeRefused("native model settings are not configured"); };
        runtime = new library.AgentRuntime(payload.storageId, ports, library.piEngineFactory({
          providers: [], credentials: { read: denied, list: denied, modify: denied, delete: denied },
        }));
        return runtime.start();
      }
      case "runtime.inspect": return requireRuntime().inspect();
      case "runtime.confirmRecovery": return requireRuntime().confirmRecovery(payload.approval as RecoveryApproval);
      case "runtime.dispatch": return requireRuntime().dispatch(payload.action as SchedulingAction, payload.arguments);
      case "runtime.observe": {
        if (typeof payload.conversationId !== "number" || !Number.isSafeInteger(payload.conversationId) || payload.conversationId < 0) {
          throw new library.RuntimeRefused("conversation identity is invalid");
        }
        const epoch = ++observationEpoch;
        observation?.detach(); observation = undefined;
        const next = await requireRuntime().observeConversation(payload.conversationId, (event) => {
          if (disposed || epoch !== observationEpoch) return;
          bridge.postMessage(JSON.stringify({ channel: library.NATIVE_CHANNEL, event: "conversation.changed", conversationId: payload.conversationId, change: event }));
        });
        if (epoch !== observationEpoch || disposed) {
          next.detach();
          throw new library.RuntimeRefused("conversation observation was superseded");
        }
        observation = next;
        return observation.value;
      }
      case "runtime.stop": {
        observationEpoch++;
        observation?.detach(); observation = undefined;
        await requireRuntime().stop();
        return null;
      }
      default: throw new library.RuntimeRefused("unreviewed native runtime command");
    }
  };
  const receive = (event: { data: string }) => {
    let envelope: { channel?: unknown; id?: unknown; payload?: unknown };
    try { envelope = JSON.parse(event.data); } catch { return; }
    if (!envelope || envelope.channel !== library.NATIVE_CHANNEL || typeof envelope.id !== "string" || !envelope.payload
      || typeof envelope.payload !== "object" || Array.isArray(envelope.payload)) return;
    const payload = envelope.payload as Record<string, unknown>;
    if (typeof payload.method !== "string" || !payload.method.startsWith("runtime.")) return;
    // Replayed commands can never start a second submission or repeat a
    // confirmation. On exhaustion retain all tombstones and fail closed.
    if (accepted.has(envelope.id) || (accepted.size >= 4096 && payload.method !== "runtime.stop")) {
      reply(envelope.id, null, new library.RuntimeRefused("runtime command was repeated or its admission limit was reached"));
      if (accepted.size >= 4096) void runtime?.stop().catch(() => {});
      return;
    }
    if (accepted.size < 4096) accepted.add(envelope.id);
    const id = envelope.id;
    void command(payload).then((result) => { reply(id, result); phase(); }, (error: unknown) => { reply(id, null, error); phase(); });
    phase();
  };
  bridge.addEventListener("message", receive);
  bridge.postMessage(JSON.stringify({ channel: library.NATIVE_CHANNEL, event: "runtime.loaded", version: "0.1.0", modelTransport: "unconfigured" }));
  return () => {
    if (disposed) return;
    disposed = true;
    bridge.removeEventListener("message", receive);
    observationEpoch++;
    observation?.detach();
    // Native destruction independently revokes and retains the lock until
    // native operations drain; a web document cannot guarantee teardown.
    if (runtime) void runtime.stop().finally(() => wire.close()).catch(() => {});
    else wire.close();
  };
}

const host = globalThis as unknown as HostWindow;
if (host.BKCatNative && host.BKCatRuntime) {
  const detach = attachNativeController(host.BKCatRuntime, host.BKCatNative);
  host.addEventListener("pagehide", detach, { once: true });
}
