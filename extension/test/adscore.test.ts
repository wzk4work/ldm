import { test } from "node:test";
import assert from "node:assert/strict";
import {
  adFields,
  isOpaqueId,
  AD_THRESHOLD, AD_WEIGHTS, adTokenIn, describeVerdict, domainListed, isAdRequestUrl, parseDomainList, rankOrder, scoreAll, tokens, type PageSignals, type RankInput,
} from "../src/shared/adscore";

const one = (it: RankInput, page?: PageSignals, extra: RankInput[] = [], userDomains: string[] = []) => scoreAll([it, ...extra], { page, userDomains })[0];

test("tokens split on punctuation and letter/digit boundaries", () => {
  assert.deepEqual(tokens("/x/ads123/Pre-Roll_v2.mp4"), ["x", "ads", "123", "pre", "roll", "v", "2", "mp", "4"]);
});

test("ad words are whole tokens only (no 'download', 'header', 'upload', 'head' hits)", () => {
  assert.equal(adTokenIn("https://cdn.example.com/ads/clip.mp4"), "ads");
  assert.equal(adTokenIn("https://cdn.example.com/v/preroll_15s.mp4"), "preroll");
  assert.equal(adTokenIn("https://cdn.example.com/v/pre-roll.mp4"), "pre-roll");
  assert.equal(adTokenIn("https://cdn.example.com/vast/creative/123.mp4"), "vast");
  for (const ok of ["https://cdn.example.com/download.mp4", "https://e.com/header/video.mp4", "https://e.com/upload/head.mp4", "https://e.com/adobe/padded.mp4", "https://e.com/v.mp4?ad=1"])
    assert.equal(adTokenIn(ok), null, ok);
});

test("domain lists: built-in, subdomains, user file (incl. hosts format)", () => {
  assert.equal(domainListed("pubads.g.doubleclick.net", ["doubleclick.net"]), "doubleclick.net");
  assert.equal(domainListed("notdoubleclick.net", ["doubleclick.net"]), null);
  assert.deepEqual(parseDomainList("# mine\nads.example.org\n0.0.0.0 track.example.net  # hosts\n*.promo.example.com\nlocalhost\nnot a domain\n"), [
    "ads.example.org", "track.example.net", "promo.example.com",
  ]);
  assert.ok(one({ url: "https://s0.2mdn.net/v/x.mp4", kind: "progressive" }).reasons.includes("ad-domain:2mdn.net"));
  // seen as an outstream filler video next to the Dailymotion player (Firefox real-site run)
  assert.ok(one({ url: "https://static.smilewanted.com/video/video_vide.mp4", kind: "progressive" }).reasons.includes("ad-domain:smilewanted.com"));
  const u = one({ url: "https://media.myads.example/v.mp4", kind: "progressive" }, undefined, [], ["myads.example"]);
  assert.equal(u.score, AD_WEIGHTS.userAdDomain);
  assert.ok(u.ad);
  assert.ok(one({ url: "https://ads.example.com/v.mp4", kind: "progressive" }).reasons[0].startsWith("ad-subdomain"));
});

test("VAST / VMAP / IMA request detection", () => {
  assert.ok(isAdRequestUrl("https://x.example.com/vast/3.0?id=1"));
  assert.ok(isAdRequestUrl("https://imasdk.googleapis.com/js/sdkloader/ima3.js"));
  assert.ok(isAdRequestUrl("https://ads.example.com/serve?vmap=1"));
  assert.ok(!isAdRequestUrl("https://www.example.com/vastness/video.m3u8"));
});

test("format signal: small progressive beside a manifest, on another host", () => {
  const hls = { url: "https://cdn.example.com/hls/master.m3u8", kind: "hls" as const };
  const v = one({ url: "https://promo-cdn.example.com/x/clip.mp4", kind: "progressive", size: 2_000_000 }, undefined, [hls]);
  assert.deepEqual(v.reasons, ["progressive-beside-manifest", "other-host:promo-cdn.example.com"]);
  assert.equal(v.score, 2.0);
  assert.ok(!v.ad, "format alone is not enough to grey");
  // a big progressive (the real file) next to a manifest does not get the point
  assert.deepEqual(one({ url: "https://cdn.example.com/full.mp4", kind: "progressive", size: 900_000_000 }, undefined, [hls]).reasons, []);
});

test("duration signals: short, mismatch, match with page-declared duration", () => {
  assert.ok(one({ url: "https://e.com/a.mp4", kind: "progressive", duration: 15 }).reasons.includes("short:15s"));
  const m = one({ url: "https://e.com/a.m3u8", kind: "hls", duration: 64 }, { declaredDuration: 65 });
  assert.ok(m.reasons.includes("duration-match:64s"));
  assert.equal(m.score, AD_WEIGHTS.manifest + AD_WEIGHTS.durationMatch);
  const mm = one({ url: "https://e.com/b.mp4", kind: "progressive", duration: 120 }, { declaredDuration: 600 });
  assert.ok(mm.reasons.includes("duration-mismatch:120s/600s"));
  // the player's own duration counts for progressive files
  const p = one({ url: "https://e.com/c.mp4", kind: "progressive" }, { players: [{ src: "https://e.com/c.mp4", duration: 20, area: 1, played: 5 }] });
  assert.ok(p.reasons.includes("short:20s"));
});

test("timing / player / skip-control / ad-request signals", () => {
  const ad = "https://adcdn.example.net/pre_videos/1/mp4_720.mp4";
  const main = "https://cdn.example.com/hls/master.m3u8";
  const page: PageSignals = {
    players: [
      { src: ad, duration: 20, area: 600 * 340, played: 8, firstPlayAt: 1000, lastPlayAt: 9000, duringSkip: true },
      { src: "blob:https://www.example.com/1", duration: 65, area: 600 * 340, played: 30, firstPlayAt: 12000, lastPlayAt: 42000 },
    ],
    skipWindows: [[5000, 11000]],
    adRequests: [500],
  };
  const [a, m] = scoreAll(
    [
      { url: ad, kind: "progressive", size: 3e6, firstSeen: 900 },
      // the main stream is requested while the pre-roll's "Skip Ad" is still shown (as on the real site)
      { url: main, kind: "hls", firstSeen: 10500, duration: 65 },
    ],
    { page },
  );
  for (const r of ["progressive-beside-manifest", "other-host:adcdn.example.net", "short:20s", "early-short", "skip-control", "ad-request"]) assert.ok(a.reasons.includes(r), r);
  assert.ok(a.ad && a.score >= AD_THRESHOLD);
  assert.deepEqual(m.reasons, ["manifest", "duration-match:65s", "main-player"]);
  assert.ok(!m.ad && m.score < 0);
  // skip window without a player mapping is a weaker hint
  const w = one({ url: "https://e.com/x.mp4", kind: "progressive", firstSeen: 4000 }, { skipWindows: [[5000, 9000]] });
  assert.deepEqual(w.reasons, ["skip-window"]);
});

test("a plain page with one video is never greyed", () => {
  const v = one({ url: "https://www.example.com/media/download.mp4", kind: "progressive", size: 5e6, firstSeen: 1 }, {
    players: [{ src: "https://www.example.com/media/download.mp4", duration: 20, area: 640 * 360, played: 20, firstPlayAt: 1, lastPlayAt: 20000 }],
  });
  assert.ok(!v.ad, JSON.stringify(v));
  assert.ok(v.reasons.includes("main-player"));
});

test("ranking: main first, suspected ads last, input order kept for ties; off switch", () => {
  const items: RankInput[] = [
    { url: "https://ads.example.com/a.mp4", kind: "progressive" },
    { url: "https://cdn.example.com/master.m3u8", kind: "hls" },
    { url: "https://cdn.example.com/other.mp4", kind: "progressive", size: 9e8 },
    { url: "https://s0.2mdn.net/b.mp4", kind: "progressive" },
  ];
  const v = scoreAll(items);
  assert.deepEqual(rankOrder(v), [1, 2, 0, 3]);
  assert.deepEqual(v.map((x) => x.ad), [true, false, false, true]);
  const off = scoreAll(items, { enabled: false });
  assert.deepEqual(rankOrder(off), [0, 1, 2, 3]);
  assert.ok(off.every((x) => !x.ad && x.reasons.length === 0));
  assert.match(describeVerdict(v[3], "zh"), /^广告评分 5\n· 广告域名 \(2mdn\.net\)\n· 页面另有 HLS\/DASH 流/);
});

import { directDownloadItem, menuEntries, parseDuration, SKIP_CONTROL_RE } from "../src/shared/adscore";

test("page-declared durations: seconds, ISO-8601", () => {
  assert.equal(parseDuration("65"), 65);
  assert.equal(parseDuration("PT1M5S"), 65);
  assert.equal(parseDuration("PT1H"), 3600);
  assert.equal(parseDuration("P0DT0H2M"), 120);
  assert.equal(parseDuration("soon"), null);
  assert.equal(parseDuration(""), null);
});

test("skip-ad control texts", () => {
  for (const s of ["Skip Ad", "skip ads ›", "Skip in 5", "跳过广告", "Ad 1 of 2", "Ad · 0:15", "广告 15"]) assert.ok(SKIP_CONTROL_RE.test(s), s);
  for (const s of ["Skip to content", "Download", "Advanced settings", "Head", "Skipping stones tutorial"]) assert.ok(!SKIP_CONTROL_RE.test(s), s);
});

test("menu rows: main first, ads last + greyed + tagged, tooltip with reasons", () => {
  const rows = menuEntries(
    [
      { url: "https://adcdn.example.net/a.mp4", kind: "progressive", ad: true, adScore: 5.5, adReasons: ["skip-control", "short:20s"] },
      { url: "https://cdn.example.com/master.m3u8", kind: "hls", ad: false, adScore: -5.5, adReasons: ["manifest", "main-player"] },
      { url: "https://cdn.example.com/b.mp4", kind: "progressive" },
    ],
    "zh",
  );
  assert.deepEqual(rows.map((r) => [r.item.url.split("/").pop(), r.greyed, r.tag]), [
    ["master.m3u8", false, null],
    ["b.mp4", false, null],
    ["a.mp4", true, "疑似广告"],
  ]);
  assert.match(rows[2].tooltip, /广告评分 5\.5\n· 播放时有“跳过广告”按钮\n· 时长很短 \(20s\)/);
  assert.equal(rows[1].tooltip, "https://cdn.example.com/b.mp4");
  assert.equal(menuEntries([{ url: "u", kind: "hls", ad: true }], "en")[0].tag, "Ad?");
});

test("page button reads the ad flag only from a real verdict (flat or nested), never from any truthy value", () => {
  assert.deepEqual(adFields({ ad: true, adScore: 4, adReasons: ["skip-control"] }), { ad: true, adScore: 4, adReasons: ["skip-control"] });
  // nested verdict (daemon form): {ad:false,...} is an object, but must not grey the row
  assert.deepEqual(adFields({ ad: { ad: false, score: -5, reasons: ["main-player"] } }), { ad: false, adScore: -5, adReasons: ["main-player"] });
  assert.equal(adFields({ ad: { ad: true, score: 3, reasons: [] } }).ad, true);
  assert.equal(adFields({ ad: "yes" }).ad, false);
  assert.equal(adFields({ ad: null }).ad, false);
  assert.equal(adFields({}).ad, false);
});

test("opaque path ids are not words (real-world: Vimeo CDN ids contain '…6ad1')", () => {
  const u = "https://skyfire.vimeocdn.com/1792307690-0x5d6f/ccfaa6de-0af0-44cb-a61a-8ecebadb6ad1/v2/playlist/av/primary/playlist.mpd";
  assert.equal(adTokenIn(u), null);
  assert.ok(isOpaqueId("8ecebadb6ad1") && isOpaqueId("ccfaa6de") && !isOpaqueId("ads123") && !isOpaqueId("preroll2"));
  assert.equal(adTokenIn("https://e.com/ads123/clip.mp4"), "ads");
  assert.equal(adTokenIn("https://e.com/v/preroll2.mp4"), "preroll");
});

test("button one-click download only for a single non-ad plain file; an ad-like single candidate opens the menu", () => {
  const f = { url: "https://cdn.example.com/v.mp4", kind: "progressive" };
  assert.equal(directDownloadItem([f]), f);
  // Firefox real-site run (Dailymotion): the only candidate was an ad network's filler clip
  assert.equal(directDownloadItem([{ url: "https://static.smilewanted.com/video/video_vide.mp4", kind: "progressive", ad: true }]), null);
  assert.equal(directDownloadItem([{ url: "https://cdn.example.com/m.m3u8", kind: "hls" }]), null);
  assert.equal(directDownloadItem([f, { url: "https://cdn.example.com/w.mp4", kind: "progressive" }]), null);
  assert.equal(directDownloadItem([]), null);
});
