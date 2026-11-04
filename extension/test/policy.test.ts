import { test } from "node:test";
import assert from "node:assert/strict";
import { adapterMatchesUrl, adaptersFor, ClickLog, eligibleRect, hostListed, modifierAction, proxyInfoFrom, qualityLabel, sortQualities } from "../src/shared/policy";
import { filterLinks } from "../src/shared/picker";

test("modifier keys: bypass wins over force, 'none' disables", () => {
  assert.equal(modifierAction({ alt: true }, "alt", "insert"), "bypass");
  assert.equal(modifierAction({ insert: true }, "alt", "insert"), "force");
  assert.equal(modifierAction({ alt: true, insert: true }, "alt", "insert"), "bypass");
  assert.equal(modifierAction({ shift: true }, "alt", "insert"), null);
  assert.equal(modifierAction({ alt: true }, "none", "insert"), null);
  assert.equal(modifierAction(undefined, "alt", "insert"), null);
});

test("click log matches the download URL within the TTL", () => {
  const log = new ClickLog(1000);
  log.add({ url: "http://h/a.zip", mods: { alt: true }, tabId: 3, at: 100 });
  assert.equal(log.find(["http://x/", "http://h/a.zip"], 500)?.tabId, 3);
  assert.equal(log.find(["http://h/a.zip"], 2000), undefined);
});

test("site lists", () => {
  assert.ok(hostListed("www.bank.example", ["bank.example"]));
  assert.ok(hostListed("bank.example", ["*.bank.example"]));
  assert.ok(!hostListed("notbank.example", ["bank.example"]));
  assert.ok(!hostListed("", ["x"]));
});

test("site adapters: host match, url patterns", () => {
  const ad = [{ id: "p", matchHosts: ["*.video.example"], detect: { urlPatterns: ["**/playlist.m3u8*"], elementSelectors: ["#player"] } }, { bogus: 1 }];
  const a = adaptersFor("cdn.video.example", ad);
  assert.equal(a.length, 1);
  assert.equal(adaptersFor("video.example", ad).length, 1);
  assert.equal(adaptersFor("other.example", ad).length, 0);
  assert.ok(adapterMatchesUrl("https://cdn.video.example/x/playlist.m3u8?t=1", a));
  assert.ok(!adapterMatchesUrl("https://cdn.video.example/x/seg1.ts", a));
});

test("quality labels and order", () => {
  const qs = sortQualities([
    { label: "a", kind: "audio", bandwidth: 128000 },
    { label: "v1", kind: "video", height: 360, bandwidth: 800000 },
    { label: "v2", kind: "video", height: 1080, bandwidth: 5200000 },
  ]);
  assert.deepEqual(qs.map((q) => q.label), ["v2", "v1", "a"]);
  assert.equal(qualityLabel(qs[0]), "1080p · 5.2 Mbps");
  assert.equal(qualityLabel(qs[1]), "360p · 800 kbps");
  assert.equal(qualityLabel(qs[2], "仅音频"), "仅音频 · 128 kbps");
});

test("button eligibility: ≥120×80 and on screen", () => {
  const r = (x: number, y: number, w: number, h: number) => ({ left: x, top: y, width: w, height: h, right: x + w, bottom: y + h });
  assert.ok(eligibleRect(r(0, 0, 640, 360), 1280, 720));
  assert.ok(!eligibleRect(r(0, 0, 100, 360), 1280, 720));
  assert.ok(!eligibleRect(r(0, 0, 640, 60), 1280, 720));
  assert.ok(!eligibleRect(r(0, 800, 640, 360), 1280, 720));
});

test("link picker filters", () => {
  const links = [{ url: "http://h/a.zip", text: "Archive" }, { url: "http://h/page.html", text: "About" }, { url: "http://h/b.pdf" }, { url: "http://h/dir/" }];
  assert.equal(filterLinks(links, "", "").length, 4);
  assert.deepEqual(filterLinks(links, "files", "").map((l) => l.url), ["http://h/a.zip", "http://h/b.pdf"]);
  assert.deepEqual(filterLinks(links, "pdf", "").map((l) => l.url), ["http://h/b.pdf"]);
  assert.deepEqual(filterLinks(links, "", "archive").map((l) => l.url), ["http://h/a.zip"]);
});

test("browser proxy settings map to ProxyInfo", () => {
  assert.deepEqual(proxyInfoFrom({ mode: "direct" }), { mode: "direct", url: null });
  assert.deepEqual(proxyInfoFrom({ mode: "fixed_servers", rules: { singleProxy: { scheme: "socks5", host: "127.0.0.1", port: 9050 } } }), {
    mode: "fixed",
    url: "socks5://127.0.0.1:9050",
  });
  assert.deepEqual(proxyInfoFrom({ mode: "fixed_servers", rules: { proxyForHttps: { host: "p" } } }), { mode: "fixed", url: "http://p:8080" });
  assert.equal(proxyInfoFrom({ mode: "pac_script" }).mode, "pac");
  assert.equal(proxyInfoFrom(undefined).mode, "system");
});
