import { test } from "node:test";
import assert from "node:assert/strict";
import { ACK_EVENT, CH_EVENT, HS_EVENT, HS_POKE, MAX_QUEUED, hookSink, isolatedChannel, mainChannel, toJson } from "../src/shared/bridge";

class MouseEvent extends Event {
  relatedTarget: unknown;
  constructor(type: string, init: { relatedTarget?: unknown } = {}) {
    super(type);
    this.relatedTarget = init.relatedTarget ?? null;
  }
}

/** a window-like EventTarget that records every event type the page could see on it */
function fakeWin() {
  const win = new EventTarget() as EventTarget & Record<string, unknown>;
  const nodes: EventTarget[] = [];
  class Document {
    createElement() {
      const n = new EventTarget();
      nodes.push(n);
      return n;
    }
  }
  Object.assign(win, { EventTarget, CustomEvent, MouseEvent, Document, document: new Document() });
  return { win, nodes };
}

test("MAIN first, ISOLATED later: queued reports are delivered in order after the ACK", () => {
  const { win } = fakeWin();
  const post = mainChannel(win);
  post({ a: 1 });
  post({ a: 2 });
  const got: unknown[] = [];
  isolatedChannel(win, (m) => got.push(m));
  post({ a: 3 });
  assert.deepEqual(got, [{ a: 1 }, { a: 2 }, { a: 3 }]);
});

test("ISOLATED first, MAIN later: the MAIN handshake binds directly", () => {
  const { win } = fakeWin();
  const got: unknown[] = [];
  isolatedChannel(win, (m) => got.push(m));
  const post = mainChannel(win);
  post("x");
  assert.deepEqual(got, ["x"]);
});

test("the page never sees a report: no window events, no 'message', node unreachable", () => {
  const { win, nodes } = fakeWin();
  const seen: string[] = [];
  const realDispatch = EventTarget.prototype.dispatchEvent;
  // a page that hooked window.dispatchEvent / listened for 'message' after our setup
  const post = mainChannel(win);
  isolatedChannel(win, () => undefined);
  win.addEventListener("message", () => seen.push("message"));
  for (const t of [HS_EVENT, HS_POKE, CH_EVENT, ACK_EVENT]) win.addEventListener(t, () => seen.push(t), true);
  (win as Record<string, unknown>).dispatchEvent = function (e: Event) {
    seen.push("dispatch:" + e.type);
    return realDispatch.call(this, e);
  };
  for (let i = 0; i < 5; i++) post({ i });
  assert.deepEqual(seen, [], "reports travel only on the detached node");
  assert.equal(nodes.length, 1);
});

test("after the ACK a page cannot re-run the handshake to obtain the node", () => {
  const { win } = fakeWin();
  mainChannel(win);
  isolatedChannel(win, () => undefined);
  let leaked: unknown = null;
  win.addEventListener(HS_EVENT, (e) => (leaked = (e as MouseEvent).relatedTarget), true);
  win.dispatchEvent(new Event(HS_POKE)); // page pokes
  assert.equal(leaked, null);
});

test("a forged handshake after binding is ignored; malformed details are dropped", () => {
  const { win } = fakeWin();
  const got: unknown[] = [];
  isolatedChannel(win, (m) => got.push(m));
  const post = mainChannel(win);
  const evil = new EventTarget();
  win.dispatchEvent(new MouseEvent(HS_EVENT, { relatedTarget: evil }));
  evil.dispatchEvent(new CustomEvent(CH_EVENT, { detail: JSON.stringify({ forged: true }) }));
  post({ ok: 1 });
  assert.deepEqual(got, [{ ok: 1 }]);
});

test("unacknowledged queue is bounded", () => {
  const { win } = fakeWin();
  const post = mainChannel(win);
  for (let i = 0; i < MAX_QUEUED + 50; i++) post(i);
  const got: unknown[] = [];
  isolatedChannel(win, (m) => got.push(m));
  assert.equal(got.length, MAX_QUEUED);
});

test("mainChannel never throws on a broken window", () => {
  assert.doesNotThrow(() => mainChannel({})("x"));
  assert.doesNotThrow(() => isolatedChannel({}, () => undefined));
});

test("hookSink is created once per global", () => {
  const g: Record<string, unknown> = {};
  const a = hookSink(g);
  a.q.push(1);
  assert.equal(hookSink(g), a);
});

test("patched Function.prototype.call / apply, JSON.stringify and toJSON never see the node or a report", () => {
  const { win } = fakeWin();
  const got: unknown[] = [];
  const post = mainChannel(win);
  isolatedChannel(win, (m) => got.push(m));
  const seen: unknown[] = [];
  const saved = { call: Function.prototype.call, apply: Function.prototype.apply, stringify: JSON.stringify };
  /* eslint-disable @typescript-eslint/no-explicit-any */
  (Function.prototype as any).call = function (this: any, self: unknown, ...a: unknown[]) {
    seen.push(self);
    return Reflect.apply(this, self, a);
  };
  (Function.prototype as any).apply = function (this: any, self: unknown, a: unknown[]) {
    seen.push(self);
    return Reflect.apply(this, self, a ?? []);
  };
  JSON.stringify = ((v: unknown) => (seen.push(v), saved.stringify(v))) as typeof JSON.stringify;
  (Object.prototype as any).toJSON = function () {
    seen.push(this);
    return {};
  };
  (Array.prototype as any).toJSON = function () {
    seen.push(this);
    return [];
  };
  try {
    post({ url: "https://a/b.m3u8", list: ["x", 1, true, null], n: { k: 2 } });
  } finally {
    Function.prototype.call = saved.call;
    Function.prototype.apply = saved.apply;
    JSON.stringify = saved.stringify;
    delete (Object.prototype as any).toJSON;
    delete (Array.prototype as any).toJSON;
  }
  assert.deepEqual(got, [{ url: "https://a/b.m3u8", list: ["x", 1, true, null], n: { k: 2 } }]);
  assert.deepEqual(seen.filter((x) => x && typeof x === "object"), [], "no page hook saw an object of ours");
});

test("toJson matches JSON.stringify for our report shapes", () => {
  for (const v of [{ a: 1, b: "x\"y", c: [1, 2, { d: null }], e: undefined, f: false }, [], {}, "s", 3.5, NaN, null])
    assert.equal(toJson(v), JSON.stringify(v));
});
