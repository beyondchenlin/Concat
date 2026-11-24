// SPDX-License-Identifier: AGPL-3.0-or-later
import assert from "node:assert/strict";
import test from "node:test";
import { NativeWireClient, NATIVE_CHANNEL, type NativeMessageBridge } from "../src/native-wire.ts";

test("save acknowledgements exceed the former lifetime count while retaining all duplicate and generation checks", async () => {
  let listener: ((event: { data: string }) => void) | undefined;
  let acknowledgements = 0;
  let failures = 0;
  const owner = { storageId: "fixed", handle: "native-owner", generation: "7" };
  const receive = (value: unknown) => listener?.({ data: JSON.stringify(value) });
  const bridge: NativeMessageBridge = {
    addEventListener(_type, value) { listener = value; },
    removeEventListener(_type, value) { if (listener === value) listener = undefined; },
    postMessage(serialized) {
      const frame = JSON.parse(serialized);
      if (frame.event === "operation.failed") { failures++; return; }
      assert.equal(frame.payload.method, "operation.acknowledge");
      assert.equal(frame.payload.ownerHandle, owner.handle);
      assert.equal(frame.payload.generation, owner.generation);
      acknowledgements++;
      receive({ channel: NATIVE_CHANNEL, id: frame.id, result: null });
    },
  };
  const wire = new NativeWireClient(bridge);
  wire.bindOwner(owner);
  const saved = { channel: NATIVE_CHANNEL, event: "operation.saved", ownerHandle: owner.handle, generation: owner.generation };
  for (let index = 1; index <= 65537; index++) receive({ ...saved, operationId: String(index) });
  await Promise.resolve();
  assert.equal(acknowledgements, 65537);
  assert.equal(failures, 0);
  receive({ ...saved, operationId: "1" });
  receive({ ...saved, operationId: "65537" });
  receive({ ...saved, operationId: "65538", generation: "6" });
  assert.equal(acknowledgements, 65537);
  receive({ channel: NATIVE_CHANNEL, event: "owner.revoked", ownerHandle: owner.handle, generation: owner.generation });
  receive({ ...saved, operationId: "65538" });
  assert.equal(acknowledgements, 65537);
  wire.close();
});
