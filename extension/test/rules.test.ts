import { test } from "node:test";
import assert from "node:assert/strict";
import { CompiledRules, globToRegExp } from "../src/shared/rules";
import { DEFAULT_RULES } from "../src/shared/protocol";

test("glob compilation", () => {
  assert.ok(globToRegExp("*.example.com").test("cdn.example.com"));
  assert.ok(!globToRegExp("*.example.com").test("example.org"));
  assert.ok(globToRegExp("**/playlist.m3u8*").test("https://v.example.com/a/b/playlist.m3u8?t=1"));
  assert.ok(globToRegExp("a?c").test("abc"));
});

test("shouldCapture honours excludes, size floor and schemes", () => {
  const r = new CompiledRules({ ...DEFAULT_RULES, excludeHosts: ["*.internal.test"], excludeUrlPatterns: ["*/nocapture/*"] });
  assert.ok(r.shouldCapture("https://dl.example.com/file.zip", null, 5_000_000));
  assert.ok(!r.shouldCapture("https://dl.example.com/file.zip", null, 100), "below min size");
  assert.ok(!r.shouldCapture("https://a.internal.test/file.zip"));
  assert.ok(!r.shouldCapture("https://internal.test/file.zip"));
  assert.ok(!r.shouldCapture("https://h/nocapture/file.zip"));
  assert.ok(!r.shouldCapture("blob:https://h/uuid"));
  assert.ok(!r.shouldCapture("https://h/page.html", "text/html"));
  assert.ok(r.shouldCapture("https://h/get?id=1", "application/zip", null, "archive.zip"));
  const off = new CompiledRules({ ...DEFAULT_RULES, captureEnabled: false });
  assert.ok(!off.shouldCapture("https://dl.example.com/file.zip"));
});
