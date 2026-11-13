// SPDX-License-Identifier: AGPL-3.0-or-later

import type { ExecutionGrant, OwnerLease, RuntimePhase } from "./ports.ts";

export class RuntimeRefused extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RuntimeRefused";
  }
}

/** Lifecycle authority only. Pi remains the sole conversation scheduler. */
export class ExecutionGate {
  #phase: RuntimePhase = "new";
  #lease: OwnerLease | undefined;
  #grant: ExecutionGrant | undefined;
  readonly #abort = new AbortController();

  get phase(): RuntimePhase { return this.#phase; }
  get signal(): AbortSignal { return this.#abort.signal; }

  acquiring(): void {
    if (this.#phase !== "new") throw new RuntimeRefused("runtime was already started");
    this.#phase = "acquiring";
  }

  owned(lease: OwnerLease): void {
    if (this.#phase !== "acquiring") throw new RuntimeRefused("runtime stopped while acquiring ownership");
    if (!lease.storageId || !lease.handle || !/^[1-9][0-9]*$/.test(lease.generation)) {
      throw new RuntimeRefused("native ownership response is invalid");
    }
    this.#lease = Object.freeze({ ...lease });
    this.#phase = "recovery";
  }

  assertOwned(lease: OwnerLease): void {
    if (!this.#lease || this.#lease.storageId !== lease.storageId
      || this.#lease.generation !== lease.generation || this.#lease.handle !== lease.handle
      || !["recovery", "activating", "ready"].includes(this.#phase)) {
      throw new RuntimeRefused("native host generation is no longer active");
    }
    this.signal.throwIfAborted();
  }

  activating(lease: OwnerLease, grant: ExecutionGrant): void {
    this.assertOwned(lease);
    if (this.#phase !== "recovery" || grant.generation !== lease.generation) {
      throw new RuntimeRefused("recovery approval does not belong to this host generation");
    }
    this.#grant = Object.freeze({ ...grant });
    this.#phase = "activating";
  }

  ready(lease: OwnerLease): void {
    this.assertOwned(lease);
    if (this.#phase !== "activating") throw new RuntimeRefused("runtime is not activating");
    this.#phase = "ready";
  }

  assertReady(lease: OwnerLease): void {
    this.assertOwned(lease);
    if (this.#phase !== "ready") throw new RuntimeRefused("user reconciliation is required before scheduling");
  }

  assertStorageWrite(lease: OwnerLease): void {
    this.assertOwned(lease);
    if (!this.#grant || !["activating", "ready"].includes(this.#phase)) {
      throw new RuntimeRefused("writable storage requires the native recovery grant");
    }
  }

  networkPermit(lease: OwnerLease): string {
    this.assertReady(lease);
    if (!this.#grant?.networkPermit) throw new RuntimeRefused("no approved model channel and budget");
    return this.#grant.networkPermit;
  }

  stop(): void {
    if (this.#phase === "closed" || this.#phase === "stopping") return;
    this.#phase = "stopping";
    this.#abort.abort(new RuntimeRefused("native runtime stopped"));
  }

  closed(): void {
    this.stop();
    this.#phase = "closed";
  }
}

export function combinedSignal(first: AbortSignal, second?: AbortSignal | null): AbortSignal {
  return second ? AbortSignal.any([first, second]) : first;
}
