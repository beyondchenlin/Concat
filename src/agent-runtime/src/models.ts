// SPDX-License-Identifier: AGPL-3.0-or-later

// Use the public core module directly. With lazy API chunks, importing this
// function through Pi's side-effect-free root barrel lets the browser bundler
// omit the core module's initializer while retaining the hoisted function.
import { createModels } from "@earendil-works/pi-ai/models";
import type { CredentialStore, Models, Provider } from "@earendil-works/pi-ai";
import { combinedSignal, ExecutionGate, RuntimeRefused } from "./gate.ts";
import type { NativePorts, OwnerLease } from "./ports.ts";
import type { NativeModelConfiguration } from "./model-configuration.ts";

/** Supplied only from the native user's explicit project model settings. */
export interface ModelConfiguration {
  readonly providers: readonly Provider[];
  /** The native implementation scopes access to this product and lease. */
  readonly credentials: CredentialStore;
  readonly native?: NativeModelConfiguration;
  readonly pricingKnown?: false;
}

/**
 * Keep Pi's original model resolution/streaming. Only admission, transport
 * injection and credential scope are host policy. No model loop is added.
 */
export function nativeModels(
  configuration: ModelConfiguration, ports: NativePorts, gate: ExecutionGate, lease: OwnerLease,
): Models {
  const providerIds = new Set(configuration.providers.map((provider) => provider.id));
  const credentials: CredentialStore = {
    async read(providerId, options) {
      gate.assertReady(lease);
      if (!providerIds.has(providerId)) throw new RuntimeRefused("model provider is not configured for this product");
      const signal = combinedSignal(gate.signal, options?.signal);
      const credential = await configuration.credentials.read(providerId, { ...options, signal });
      gate.assertReady(lease);
      if (credential && credential.type !== "api_key") {
        throw new RuntimeRefused("this runtime has no reviewed native OAuth transport");
      }
      return credential;
    },
    async list() { throw new RuntimeRefused("credential enumeration belongs to native settings"); },
    async modify() { throw new RuntimeRefused("credential changes belong to native settings"); },
    async delete() { throw new RuntimeRefused("credential changes belong to native settings"); },
  };
  const models = createModels({
    credentials,
    // Never search process.env, another app's key store, or the filesystem.
    authContext: { env: async () => undefined, fileExists: async () => false },
  });
  for (const provider of configuration.providers) models.setProvider(provider);

  const readMethods = new Set([
    "getProviders", "getProvider", "getModels", "getModel", "getModelsOfType", "getModelOfType", "getAllModels",
  ]);
  const optionAt: Record<string, number> = {
    stream: 2, complete: 2, streamSimple: 2, completeSimple: 2,
    streamDeferred: 2, fetchDeferred: 2, cancelDeferred: 2, generateImages: 2, classify: 2,
    getAuth: 1, checkAuth: 1, getAvailable: 1, getAvailableOfType: 2, getAllAvailable: 1,
  };
  const fetch = async (input: RequestInfo | URL, init: RequestInit = {}): Promise<Response> => {
    const networkPermit = gate.networkPermit(lease);
    const signal = combinedSignal(gate.signal, init.signal);
    signal.throwIfAborted();
    const response = await ports.network.fetch(lease, networkPermit, input, { ...init, signal });
    gate.assertReady(lease);
    signal.throwIfAborted();
    return response;
  };
  const wrappedProviders = new WeakMap<Provider, Provider>();
  const requestOptions = (options: Record<string, unknown> & { signal?: AbortSignal }) => ({
    ...options, signal: combinedSignal(gate.signal, options.signal), fetch, transport: "sse", maxRetries: 0,
    ...(configuration.native ? {
      timeoutMs: configuration.native.requestTimeoutMs, maxTokens: configuration.native.maxOutputTokens,
      cacheRetention: "none",
    } : {}),
  });
  const guardProvider = (provider: Provider): Provider => {
    const existing = wrappedProviders.get(provider);
    if (existing) return existing;
    const wrapped = new Proxy(provider, {
      get(target, property) {
        const value = Reflect.get(target, property);
        if (typeof value !== "function") return value;
        if (property === "getModels" || property === "getAllModels") return (...args: unknown[]) => {
          gate.assertOwned(lease);
          return Reflect.apply(value, target, args);
        };
        if (typeof property === "string" && optionAt[property] !== undefined) return (...args: unknown[]) => {
          gate.assertReady(lease);
          const index = optionAt[property];
          const options = (args[index] ?? {}) as Record<string, unknown> & { signal?: AbortSignal };
          args[index] = requestOptions(options);
          return Reflect.apply(value, target, args);
        };
        return () => { throw new RuntimeRefused("provider operation is not reviewed for the embedded runtime"); };
      },
    });
    wrappedProviders.set(provider, wrapped);
    return wrapped;
  };
  return new Proxy(models, {
    get(target, property) {
      const value = Reflect.get(target, property);
      if (typeof value !== "function") return value;
      if (typeof property === "string" && readMethods.has(property)) return (...args: unknown[]) => {
        gate.assertOwned(lease);
        const result = Reflect.apply(value, target, args);
        if (property === "getProvider") return result ? guardProvider(result as Provider) : undefined;
        if (property === "getProviders") return (result as Provider[]).map(guardProvider);
        return result;
      };
      if (typeof property === "string" && optionAt[property] !== undefined) return (...args: unknown[]) => {
        gate.assertReady(lease);
        const index = optionAt[property];
        const options = (args[index] ?? {}) as Record<string, unknown> & { signal?: AbortSignal };
        args[index] = requestOptions(options);
        return Reflect.apply(value, target, args);
      };
      return () => { throw new RuntimeRefused("model configuration or refresh belongs to native settings"); };
    },
  });
}
