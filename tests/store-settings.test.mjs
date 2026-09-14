import test from "node:test";
import assert from "node:assert/strict";
import { registerHooks } from "node:module";

// Exercise the real store with a controllable IPC boundary, without a desktop app.
let invoke;
globalThis.__settingsTestInvoke = (...args) => invoke(...args);
registerHooks({
  resolve(specifier, context, nextResolve) {
    const stubs = {
      "@tauri-apps/api/core": "export const invoke = (...args) => globalThis.__settingsTestInvoke(...args);",
      "@tauri-apps/api/event": "export const listen = async (channel, cb) => { globalThis.__listeners ??= {}; globalThis.__listeners[channel] = cb; return () => {}; };",
      "@tauri-apps/plugin-opener": "export const openUrl = async () => {};",
    };
    if (specifier in stubs) {
      return { url: `data:text/javascript,${encodeURIComponent(stubs[specifier])}`, shortCircuit: true };
    }
    if (specifier.startsWith("./") && !specifier.endsWith(".ts") && context.parentURL?.includes("/src/lib/")) {
      return nextResolve(`${specifier}.ts`, context);
    }
    return nextResolve(specifier, context);
  },
});

const defaults = {
  locale: "zh_CN", targets: [], intervalSeconds: 30,
  barkUrl: "https://example.invalid/old", soundEnabled: true, openOnHit: "bag",
  productBarkUrls: {},
  autoAddToBag: false, bagApplecare: false,
  pickupLastName: "", pickupFirstName: "", pickupEmail: "", pickupPhone: "", pickupIdLast4: "",
};
let generation = 0;
async function setup() {
  const store = await import(`../src/lib/store.ts?case=${++generation}`);
  let persisted = { ...defaults };
  let release;
  let blockNext = false;
  const calls = [];
  const logWrites = [];
  invoke = async (command, args) => {
    // 活动日志落盘是旁路：即发即忘，不参与这里的时序断言。若把它算进 calls，
    // 任何一次 pushLog 都会让别的用例变红；断言真正要盯的是「设置写入」与
    // 「动作」谁先谁后。它自己有没有被调用，由专门的用例盯着。
    if (command === "append_activity_log") {
      logWrites.push(args?.lines ?? []);
      return;
    }
    calls.push(command);
    if (blockNext) {
      blockNext = false;
      await new Promise((resolve) => { release = resolve; });
    }
    if (command === "save_settings") persisted = { ...args.settings };
    if (command === "set_interval") persisted.intervalSeconds = Math.max(5, args.seconds);
    if (command === "set_targets") persisted.targets = args.targets;
    if (command === "set_targets") return [];
    if (command === "set_interval") return persisted.intervalSeconds;
    return { ...persisted };
  };
  await store.saveSettings(defaults);
  calls.length = 0;
  return {
    store, calls, logWrites, persisted: () => persisted,
    block: () => { blockNext = true; },
    release: () => release(),
  };
}
const tick = () => new Promise((resolve) => setImmediate(resolve));

test("connect does not overwrite events with stale IPC snapshots", async () => {
  const { store } = await setup();
  let release;
  invoke = async (command) => {
    if (command === "get_snapshot") return new Promise(resolve => { release = () => resolve([]); });
    if (command === "is_running") return false;
    if (command === "get_settings") return defaults;
    return [];
  };
  const connecting = store.connect();
  await tick();
  globalThis.__listeners["watcher://event"]({ payload: { type: "runStateChanged", running: true } });
  release();
  await connecting;
  assert.equal(store.watcherStore.getSnapshot().running, true);
});

test("connect with saved targets stays stopped until explicit manual start", async () => {
  const { store } = await setup();
  const calls = [];
  invoke = async (command) => {
    calls.push(command);
    if (command === "get_settings") return { ...defaults, targets: [{ locale: "zh_CN", storeNumber: "R390", partNumber: "TEST/A", storeTitle: "Offline", productName: "Offline" }] };
    if (command === "is_running") return false;
    return [];
  };
  await store.connect();
  assert.equal(store.watcherStore.getSnapshot().ready, true);
  assert.equal(store.watcherStore.getSnapshot().running, false);
  assert.equal(calls.includes("start_watching"), false);
  assert.equal(calls.includes("refresh_products"), false);
});

test("overlapping edits merge with the last saved settings", async () => {
  const ctx = await setup();
  ctx.block();
  const first = ctx.store.saveSettings({ barkUrl: "" });
  await tick();
  const second = ctx.store.saveSettings({ soundEnabled: false });
  const third = ctx.store.saveSettings({ openOnHit: "product" });
  await tick();
  assert.deepEqual(ctx.calls, ["save_settings"]);
  ctx.release();
  await Promise.all([first, second, third]);
  assert.deepEqual(ctx.persisted(), {
    ...defaults,
    barkUrl: "",
    soundEnabled: false,
    openOnHit: "product",
  });
});

test("interval and target commands cannot be overwritten by queued settings", async () => {
  const ctx = await setup();
  ctx.block();
  const interval = ctx.store.setIntervalSeconds(15);
  await tick();
  const targets = [{ locale: "zh_CN", storeNumber: "R390", storeTitle: "Test", partNumber: "TEST/A", productName: "Test" }];
  const updated = ctx.store.setTargets(targets);
  const saved = ctx.store.saveSettings({ soundEnabled: false });
  await tick();
  assert.deepEqual(ctx.calls, ["set_interval"]);
  ctx.release();
  await Promise.all([interval, updated, saved]);
  assert.deepEqual(ctx.persisted(), { ...defaults, intervalSeconds: 15, targets, soundEnabled: false });
});

test("test notification waits for the cleared Bark address to finish saving", async () => {
  const ctx = await setup();
  ctx.block();
  const save = ctx.store.saveSettings({ barkUrl: "" });
  await tick();
  const notification = ctx.store.testNotify();
  await tick();
  assert.deepEqual(ctx.calls, ["save_settings"]);
  ctx.release();
  await Promise.all([save, notification]);
  assert.deepEqual(ctx.calls, ["save_settings", "test_notify"]);
  assert.equal(ctx.persisted().barkUrl, "");
});

test("saveSettings reports write failure and later success", async () => {
  const ctx = await setup();
  const originalInvoke = invoke;
  invoke = async () => { throw new Error("disk unavailable"); };
  assert.equal(await ctx.store.saveSettings({ soundEnabled: false }), false);
  invoke = originalInvoke;
  assert.equal(await ctx.store.saveSettings({ barkUrl: "" }), true);
  assert.deepEqual(ctx.persisted(), { ...defaults, barkUrl: "" });
});

test("a product-specific Bark URL can be set and cleared", async () => {
  const ctx = await setup();
  const target = {
    locale: "zh_CN", storeNumber: "R390", storeTitle: "Test",
    partNumber: "TEST/A", productName: "Test Product",
  };

  assert.equal(await ctx.store.setProductBarkUrl(target, " https://api.day.app/friend "), true);
  assert.deepEqual(ctx.persisted().productBarkUrls, {
    "TEST/A": "https://api.day.app/friend",
  });

  assert.equal(await ctx.store.setProductBarkUrl(target, ""), true);
  assert.deepEqual(ctx.persisted().productBarkUrls, {});
});

test("activity log lines are also written to disk, timestamped", async () => {
  const ctx = await setup();
  await ctx.store.testNotify();
  assert.ok(ctx.logWrites.length >= 1, "每次 pushLog 都应顺带落盘一份");
  const lines = ctx.logWrites.flat();
  assert.ok(lines.length >= 1, "落盘的应当是具体日志行，而不是空批次");
  // 界面那份带 [HH:MM:SS] 前缀，落盘的必须是同一串 —— 事后对照时才发现得了
  // 「这条告警发生在几点」。少了时间戳，日志就退化成一份没用的清单。
  for (const line of lines) {
    assert.match(line, /^\[\d{2}:\d{2}:\d{2}\] \S/, `日志行缺少时间戳：${line}`);
  }
});
