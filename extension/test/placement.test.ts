import { test } from "node:test";
import assert from "node:assert/strict";
import {
  adClassToken, adFrameUrl, adLabelNear, applyParentAdFrames, chooseWinner, containerAdToken, PLACE_HYSTERESIS, PLACE_REPORT_TTL_MS, PLACE_WEIGHTS,
  scorePlacement, urlMatchesCandidate, type ElLike, type FrameReport, type PlayerFeatures,
} from "../src/shared/placement";
import { scoreAll, type PageSignals } from "../src/shared/adscore";

// ---- tiny fixture DOM: h("div", {class, id, aria}, ...children | text) ----
type Node = ElLike & { children: Node[] };
function h(tag: string, attrs: Record<string, string> = {}, ...kids: (Node | string)[]): Node {
  const el: Node = {
    tagName: tag.toUpperCase(),
    id: attrs.id ?? "",
    className: attrs.class ?? "",
    parentElement: null,
    children: [],
    get textContent(): string {
      return kids.map((k) => (typeof k === "string" ? k : k.textContent)).join("");
    },
    getAttribute: (n: string) => (n === "aria-label" ? attrs.aria ?? null : n in attrs ? attrs[n] : null),
  };
  for (const k of kids) if (typeof k !== "string") {
    k.parentElement = el;
    el.children.push(k);
  }
  return el;
}
const find = (root: Node, tag: string): Node => {
  if (root.tagName === tag.toUpperCase()) return root;
  for (const k of root.children) {
    const f = find(k, tag);
    if (f) return f;
  }
  return null as unknown as Node;
};

test("ad class/id tokens are whole tokens, not hex ids or words containing 'ad'", () => {
  for (const [s, t] of [["ad-container", "ad"], ["video-ads", "ads"], ["outstream-wrapper", "outstream"], ["AdContainer", "ad"], ["sponsoredContent", "sponsored"],
    ["pre-roll", "preroll"], ["adsbygoogle", "adsbygoogle"], ["dfp_ad_slot", "ad"], ["player preroll-layer", "preroll"]] as const)
    assert.equal(adClassToken(s)?.replace("pre-roll", "preroll"), t, s);
  for (const s of ["header", "loading", "shadow", "badge", "video-ad3f9b2c", "player-8ad21f0e", "readability", "masthead", "upload-btn", "Adobe", "iPad"])
    assert.equal(adClassToken(s), null, s);
});

test("container ad token walks up the ancestors and stops at <body>", () => {
  const dom = h("body", { class: "ad" }, h("div", { class: "outstream-unit" }, h("div", { class: "inner" }, h("video"))));
  assert.equal(containerAdToken(find(dom, "video")), "div:outstream");
  const main = h("body", { class: "ads-page" }, h("div", { id: "player-main" }, h("video")));
  assert.equal(containerAdToken(find(main, "video")), null, "a page-level body class does not count");
  const slot = h("div", { "data-ad-slot": "123" }, h("video"));
  assert.equal(containerAdToken(find(slot, "video")), "div:data-ad-slot");
});

test("an 'Ad' / '广告' label next to the player is found, long texts and invisible labels are not", () => {
  const ad = h("body", {}, h("div", { class: "wrap" }, h("video"), h("div", { class: "overlay" }, h("span", {}, "Ad · 0:15"))));
  assert.ok(adLabelNear(find(ad, "video")));
  const zh = h("body", {}, h("div", {}, h("video"), h("span", {}, "广告 5")));
  assert.ok(adLabelNear(find(zh, "video")));
  const aria = h("body", {}, h("div", {}, h("video"), h("div", { aria: "Advertisement" })));
  assert.ok(adLabelNear(find(aria, "video")));
  const main = h("body", {}, h("div", {}, h("video"), h("p", {}, "Adding subtitles: how ads work on this site")));
  assert.equal(adLabelNear(find(main, "video")), false);
  assert.equal(adLabelNear(find(ad, "video"), 3, 400, () => false), false, "hidden label ignored");
});

test("ad frame URLs: ad domains, ad subdomains, ad path words", () => {
  assert.equal(adFrameUrl("https://tpc.googlesyndication.com/safeframe/1-0-40/html/container.html"), "ad-domain:googlesyndication.com");
  assert.equal(adFrameUrl("https://ads.example.com/frame.html"), "ad-subdomain:ads.example.com");
  assert.equal(adFrameUrl("https://cdn.example.com/outstream/ads/unit.html"), "ad-token:ads");
  assert.equal(adFrameUrl("https://geo.dailymotion.com/player/x1.html?video=xavi89a"), null);
  assert.equal(adFrameUrl("about:blank"), null);
});

const vp = { vw: 1280, vh: 900 };
const base = (o: Partial<PlayerFeatures>): PlayerFeatures => ({ rect: { top: 100, left: 100, width: 640, height: 360 }, ...vp, playing: false, ...o });

test("fixture page: an outstream ad video larger than the main player, in an ad iframe, loses", () => {
  const main = scorePlacement(base({ playing: true, duration: 600, srcMatch: true, mainFrame: true, topFrame: true }));
  const ad = scorePlacement(base({ rect: { top: 500, left: 100, width: 960, height: 540 }, playing: true, duration: 15, adFrame: "ad-domain:teads.tv" }));
  assert.ok(main.score > ad.score, `${main.score} vs ${ad.score}`);
  assert.ok(ad.reasons.includes("ad-frame:ad-domain:teads.tv") && ad.reasons.includes("short:15s"));
  const reports: FrameReport[] = [
    { token: "top", ...main, at: Date.now() },
    { token: "adframe", ...ad, at: Date.now() },
  ];
  assert.equal(chooseWinner(reports), "top");
});

test("tiny / offscreen players are not eligible and score low", () => {
  const tiny = scorePlacement(base({ rect: { top: 0, left: 0, width: 100, height: 60 }, playing: true }));
  assert.equal(tiny.eligible, false);
  assert.ok(tiny.reasons.includes("tiny"));
  const off = scorePlacement(base({ rect: { top: 2000, left: 0, width: 640, height: 360 }, playing: true }));
  assert.equal(off.eligible, false);
  assert.ok(off.reasons.includes("offscreen"));
  assert.equal(chooseWinner([{ token: "a", ...off, at: Date.now() }]), null);
});

test("the only video is an ad (container + label + short): no button at all", () => {
  const p = scorePlacement(base({ playing: true, duration: 20, adContainer: "div:outstream", adLabel: true }));
  assert.ok(p.score <= 0, String(p.score));
  assert.equal(chooseWinner([{ token: "a", ...p, at: Date.now() }]), null);
});

test("a paused embed with nothing matched still gets the button (large, visible)", () => {
  const p = scorePlacement(base({}));
  assert.ok(p.eligible && p.score > 0, String(p.score));
});

test("parent-flagged ad iframes get the ad-frame points once", () => {
  const now = Date.now();
  const child: FrameReport = { token: "child", score: 5, eligible: true, reasons: ["playing", "large:40%"], frameUrl: "https://example.com/frames/x7.html", at: now };
  const parent: FrameReport = { token: "top", score: 3, eligible: true, reasons: ["large:20%"], frameUrl: "https://example.com/", adIframes: ["https://example.com/frames/x7.html#x"], at: now };
  const adj = applyParentAdFrames([child, parent]);
  assert.equal(adj[0].score, 5 + PLACE_WEIGHTS.adFrame);
  assert.ok(adj[0].reasons.includes("ad-frame:parent-ad-container"));
  assert.equal(chooseWinner([child, parent]), "top");
  const already = { ...child, reasons: ["ad-frame:ad-token:ads"], score: -1 };
  assert.equal(applyParentAdFrames([already, parent])[0].score, -1);
});

test("winner hysteresis and stale reports", () => {
  const now = Date.now();
  const a: FrameReport = { token: "a", score: 5, eligible: true, reasons: [], at: now };
  const b: FrameReport = { token: "b", score: 5 + PLACE_HYSTERESIS / 2, eligible: true, reasons: [], at: now };
  assert.equal(chooseWinner([a, b], "a", now), "a", "small lead keeps the current winner");
  assert.equal(chooseWinner([a, { ...b, score: 5 + PLACE_HYSTERESIS + 0.1 }], "a", now), "b");
  assert.equal(chooseWinner([{ ...a, at: now - PLACE_REPORT_TTL_MS - 1 }, { ...b, score: 1 }], "a", now), "b", "stale winner dropped");
});

test("an ad playing in the main player (label) still keeps the button on it when it is the only player", () => {
  const p = scorePlacement(base({ playing: true, duration: 15, adLabel: true, topFrame: true }));
  assert.ok(p.eligible);
  assert.equal(chooseWinner([{ token: "main", ...p, at: Date.now() }]), "main", String(p.score));
});

test("candidate URL matching: exact, fragment-insensitive, manifest directory for segments", () => {
  assert.ok(urlMatchesCandidate("https://c.example/v/main.mp4#t=1", { url: "https://c.example/v/main.mp4", kind: "progressive" }));
  assert.equal(urlMatchesCandidate("https://c.example/v/other.mp4", { url: "https://c.example/v/main.mp4", kind: "progressive" }), false);
  assert.ok(urlMatchesCandidate("https://c.example/hls/abc/seg-1.ts", { url: "https://c.example/hls/abc/master.m3u8", kind: "hls" }));
  assert.equal(urlMatchesCandidate("https://c.example/other/seg-1.ts", { url: "https://c.example/hls/abc/master.m3u8", kind: "hls" }), false);
  assert.equal(urlMatchesCandidate("https://c.example/seg.ts", { url: "https://c.example/master.m3u8", kind: "hls" }), false, "root dir is too broad");
});

test("ranking: a file played in an ad frame/container is not the main player and ranks as ad", () => {
  const page: PageSignals = {
    players: [
      { src: "https://cdn.example.com/v/clip-a.mp4", area: 960 * 540, played: 20, duration: 40, adContext: "ad-frame:ad-domain:teads.tv" },
      { src: "https://cdn.example.com/v/main.mp4", area: 640 * 360, played: 20, duration: 600 },
    ],
  };
  const [ad, main] = scoreAll(
    [
      { url: "https://cdn.example.com/v/clip-a.mp4", kind: "progressive" },
      { url: "https://cdn.example.com/v/main.mp4", kind: "progressive" },
    ],
    { page },
  );
  assert.ok(main.reasons.includes("main-player"), main.reasons.join());
  assert.ok(ad.reasons.some((r) => r.startsWith("ad-context:")), ad.reasons.join());
  assert.ok(ad.score > main.score);
});
