import test from "node:test";
import assert from "node:assert/strict";
import { describeAvailability, describeAdvice, isUntrusted } from "../src/lib/types.ts";

test("浏览器启动失败保留原因和重试间隔，不归为网络故障或无货", () => {
  const state = {kind: "unknown", reason: "browser_unavailable", detail: "浏览器专属资料目录被占用，或无法取得目录锁（退出码 21）"};
  const presentation = describeAvailability(state);
  assert.equal(presentation.label, "浏览器未连接");
  assert.equal(isUntrusted(state), true);
  assert.match(presentation.detail, /目录锁.*退出码 21/);
  assert.match(presentation.detail, /按设置的间隔/);
  assert.doesNotMatch(presentation.detail, /网络请求失败|无货/);
});

test("门店未接入在线取货显示为暂停服务而不是查询故障", () => {
  const state = { kind: "unknown", reason: "store_pickup_unavailable", store_number: "R384" };
  assert.equal(describeAvailability(state).label, "暂停取货");
  assert.equal(describeAvailability(state).tone, "comingSoon");
  assert.equal(isUntrusted(state), false);
});
