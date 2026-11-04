import { test } from "node:test";
import assert from "node:assert/strict";
import { HostClient, type Transport } from "../src/shared/native";
import { envelope, isEnvelope } from "../src/shared/protocol";
import { urlsInText } from "../src/shared/menus";

class FakeTransport implements Transport {
  readonly name = "fake";
  onMessage: ((m: unknown) => void) | null = null;
  onDisconnect: ((r?: string) => void) | null = null;
  sent: any[] = [];
  constructor(private auto = true) {}
  send(msg: any): void {
    this.sent.push(msg);
    if (!this.auto) return;
    queueMicrotask(() => {
      if (msg.type === "Hello") {
        this.onMessage?.({ v: 1, id: msg.id, type: "HelloAck", payload: { protocolVersion: 1, features: ["http"] } });
        this.onMessage?.({ v: 1, id: msg.id, type: "RuleSetPush", payload: { version: 2 } });
      } else if (msg.type === "DownloadAdd") {
        this.onMessage?.({ v: 1, id: 0, type: "JobProgress", payload: {} });
        this.onMessage?.({ v: 1, id: msg.id, type: "InterceptDecision", payload: { action: "take", jobId: "j1" } });
      }
    });
  }
  close(): void {
    this.onDisconnect?.("closed");
  }
}

const tick = () => new Promise((r) => setTimeout(r, 5));

test("handshake, request/response correlation and events", async () => {
  const t = new FakeTransport();
  const c = new HostClient([() => t], () => envelope("Hello", { browser: "chrome", version: "1" }), 10);
  const events: string[] = [];
  c.onEvent((e) => events.push(e.type));
  c.connect();
  await tick();
  assert.ok(c.ready);
  assert.deepEqual(events, ["HelloAck", "RuleSetPush"]);
  const r = await c.request("DownloadAdd", { url: "https://h/a.zip" });
  assert.equal(r.type, "InterceptDecision");
  assert.equal((r.payload as any).jobId, "j1");
  assert.ok(events.includes("JobProgress"));
  assert.ok(t.sent.every(isEnvelope));
  c.disconnect();
});

test("timeouts, disconnect rejects pending, fallback transport", async () => {
  const silent = new FakeTransport(false);
  const good = new FakeTransport();
  let n = 0;
  const c = new HostClient([() => (n++ === 0 ? silent : good), () => good], () => envelope("Hello", {}), 10);
  c.connect();
  await tick();
  assert.ok(!c.ready);
  const p = c.request("Ping", {}, 1000);
  silent.close(); // disconnect → pending rejected, next transport tried
  await assert.rejects(p, /disconnected/);
  await tick();
  await tick();
  assert.ok(c.ready, "fell back to the second transport");
  await assert.rejects(new HostClient([() => good], () => envelope("Hello", {})).request("X", {}), /not connected/);
  const slow = new HostClient([() => new FakeTransport(false)], () => envelope("Hello", {}), 10);
  slow.connect();
  await assert.rejects(slow.request("Ping", {}, 20), /timeout/);
  slow.disconnect();
  c.disconnect();
});

test("envelope validation and selection URL parsing", () => {
  assert.ok(isEnvelope({ v: 1, id: 1, type: "Ping", payload: {} }));
  assert.ok(!isEnvelope({ v: 2, id: 1, type: "Ping", payload: {} }));
  assert.ok(!isEnvelope({ v: 1, id: 1, type: "", payload: {} }));
  assert.ok(!isEnvelope({ v: 1, id: 1, type: "Ping", payload: [] }));
  assert.deepEqual(urlsInText("see https://a.example/x.zip, and (http://b.example/y.mp4). dup https://a.example/x.zip"), ["https://a.example/x.zip", "http://b.example/y.mp4"]);
});
