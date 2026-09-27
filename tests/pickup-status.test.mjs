import test from "node:test";
import assert from "node:assert/strict";
import { describeAvailability, describeAdvice, isUntrusted } from "../src/lib/types.ts";

test("门店未接入在线取货显示为暂停服务而不是查询故障", () => {
  const state = { kind: "unknown", reason: "store_pickup_unavailable", store_number: "R384" };
  assert.equal(describeAvailability(state).label, "暂停取货");
  assert.equal(describeAvailability(state).tone, "comingSoon");
  assert.equal(isUntrusted(state), false);
});
