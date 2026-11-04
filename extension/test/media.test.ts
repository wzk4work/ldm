import { test } from "node:test";
import assert from "node:assert/strict";
import { MediaRegistry, dedupeKey, hlsDuration, hlsFirstVariant } from "../src/shared/media";

test("dedupe by URL without fragment, merge more specific info", () => {
  const r = new MediaRegistry();
  assert.equal(dedupeKey("https://h/a.mp4#t=3"), "https://h/a.mp4");
  assert.ok(r.add(1, { url: "https://h/a.mp4#x", kind: "progressive" }, 1000));
  assert.ok(!r.add(1, { url: "https://h/a.mp4", kind: "progressive" }, 1001));
  assert.ok(r.add(1, { url: "https://h/a.mp4", kind: "progressive", mime: "video/mp4" }, 1002));
  assert.equal(r.list(1, 1003).length, 1);
  assert.equal(r.list(1, 1003)[0].mime, "video/mp4");
});

test("manifests first, ttl expiry, per-tab isolation, cap", () => {
  const r = new MediaRegistry(1000, 3);
  r.add(1, { url: "https://h/a.mp4", kind: "progressive" }, 0);
  r.add(1, { url: "https://h/m.m3u8", kind: "hls" }, 10);
  r.add(2, { url: "https://h/other.mp4", kind: "progressive" }, 10);
  assert.equal(r.list(1, 20)[0].kind, "hls");
  assert.ok(r.hasManifest(1, 20));
  assert.equal(r.list(2, 20).length, 1);
  assert.equal(r.list(1, 5000).length, 0, "expired");
  for (let i = 0; i < 5; i++) r.add(3, { url: `https://h/${i}.mp4`, kind: "progressive" }, i);
  assert.equal(r.list(3, 10).length, 3);
  const c = r.toCandidate(1, { url: "https://h/m.m3u8", kind: "hls", lastSeen: 0 }, "https://h/page");
  assert.deepEqual([c.tab_id, c.kind, c.page_url], [1, "hls", "https://h/page"]);
});

test("HLS master folds its variant playlists (real-world: Mux test stream in Chrome)", async () => {
  const { hlsChildUris } = await import("../src/shared/media");
  const master =
    '#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2149280,RESOLUTION=1280x720\nurl_0/193039199_mp4_h264_aac_hd_7.m3u8\n' +
    '#EXT-X-STREAM-INF:BANDWIDTH=246440,RESOLUTION=320x184\nurl_2/193039199_mp4_h264_aac_ld_7.m3u8\n' +
    '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",NAME="en",URI="audio/en.m3u8"\n';
  const base = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8";
  const kids = hlsChildUris(master, base)!;
  assert.deepEqual(kids.sort(), [
    "https://test-streams.mux.dev/x36xhzz/audio/en.m3u8",
    "https://test-streams.mux.dev/x36xhzz/url_0/193039199_mp4_h264_aac_hd_7.m3u8",
    "https://test-streams.mux.dev/x36xhzz/url_2/193039199_mp4_h264_aac_ld_7.m3u8",
  ]);
  assert.equal(hlsChildUris("#EXTM3U\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n", base), null, "media playlist is not a master");
  const r = new MediaRegistry();
  // the player fetched a variant before the master was inspected
  r.add(7, { url: "https://test-streams.mux.dev/x36xhzz/url_2/193039199_mp4_h264_aac_ld_7.m3u8", kind: "hls" }, 1);
  r.add(7, { url: base, kind: "hls" }, 2);
  assert.equal(r.list(7, 3).length, 2);
  assert.ok(r.needsInspection(7, base) && !r.needsInspection(7, base), "inspected once");
  assert.ok(r.setChildren(7, base, kids));
  assert.deepEqual(r.list(7, 4).map((x) => x.url), [base]);
  r.add(7, { url: "https://test-streams.mux.dev/x36xhzz/url_0/193039199_mp4_h264_aac_hd_7.m3u8", kind: "hls" }, 5);
  assert.deepEqual(r.list(7, 6).map((x) => x.url), [base], "later variants stay hidden");
  assert.ok(!r.needsInspection(7, "https://test-streams.mux.dev/x36xhzz/url_0/193039199_mp4_h264_aac_hd_7.m3u8"));
});

test("registry ranks candidates: main stream first, pre-roll ad greyed last; ranking can be switched off", () => {
  const r = new MediaRegistry();
  const ad = "https://adcdn.example.net/pre/clip_720.mp4";
  const hls = "https://cdn.example.com/hls/master.m3u8";
  r.add(1, { url: ad, kind: "progressive", size: 3e6 }, 1000);
  r.add(1, { url: hls, kind: "hls" }, 12000);
  r.setDuration(1, hls, 65);
  r.setSignals(1, {
    players: [
      { src: ad, duration: 20, area: 600 * 340, played: 8, firstPlayAt: 1100, lastPlayAt: 9000, duringSkip: true },
      { src: "blob:https://www.example.com/x", duration: 65, area: 600 * 340, played: 20, firstPlayAt: 12500, lastPlayAt: 32000 },
    ],
  });
  let l = r.list(1, 40000);
  assert.deepEqual(l.map((i) => [i.url, i.ad]), [[hls, false], [ad, true]]);
  assert.ok(l[1].adReasons!.includes("skip-control"));
  assert.ok(l[0].adReasons!.includes("main-player") && l[0].adReasons!.includes("duration-match:65s"));
  // signals from an iframe are merged, not overwriting the top frame
  r.setSignals(1, { players: [] }, 7);
  assert.equal(r.list(1, 40000)[1].ad, true);
  const c = r.toCandidate(1, r.list(1, 40000)[1], "https://www.example.com/v");
  assert.equal(c.ad_suspect, true);
  assert.ok((c.ad_reasons ?? []).length > 0);
  r.adSettings = { enabled: false, userDomains: [] };
  l = r.list(1, 40000);
  assert.deepEqual(l.map((i) => i.ad), [false, false]);
  // a page with one plain video is not greyed
  const p = new MediaRegistry();
  p.add(2, { url: "https://www.example.com/media/download.mp4", kind: "progressive", size: 5e6 });
  assert.equal(p.list(2)[0].ad, false);
});

test("hls playlist helpers: duration and first variant", () => {
  assert.equal(hlsDuration("#EXTM3U\n#EXTINF:10.0,\na.ts\n#EXTINF:5.5,\nb.ts\n#EXT-X-ENDLIST\n"), 15.5);
  assert.equal(hlsDuration("#EXTM3U\n#EXTINF:10.0,\na.ts\n"), null, "live list");
  const master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2\nhi/index.m3u8\n";
  assert.equal(hlsDuration(master), null);
  assert.equal(hlsFirstVariant(master, "https://h/x/master.m3u8"), "https://h/x/low/index.m3u8");
});

test("rank signature changes when a new reason changes the score, so the page button tooltip is refreshed", () => {
  const r = new MediaRegistry();
  const hls = "https://cdn.example.com/hls/master.m3u8";
  r.add(1, { url: hls, kind: "hls" });
  r.setDuration(1, hls, 65);
  const before = r.rankSignature(1);
  r.setSignals(1, { players: [{ src: "blob:https://www.example.com/x", duration: 65, area: 600 * 340, played: 5, firstPlayAt: 1, lastPlayAt: 5000 }] });
  assert.notEqual(r.rankSignature(1), before);
  assert.ok(r.list(1)[0].adReasons!.includes("main-player"));
  const same = r.rankSignature(1);
  r.setSignals(1, { players: [{ src: "blob:https://www.example.com/x", duration: 65, area: 600 * 340, played: 9, firstPlayAt: 1, lastPlayAt: 9000 }] });
  assert.equal(r.rankSignature(1), same, "more seconds played alone does not re-render");
});

test("a '.m3u8' whose body is a VAST/VMAP ad description is greyed, not offered as the main stream (real-world: Dailymotion)", async () => {
  const { playlistBodyKind } = await import("../src/shared/media");
  assert.equal(playlistBodyKind("#EXTM3U\n#EXT-X-VERSION:3\n"), null);
  assert.equal(playlistBodyKind("\uFEFF#EXTM3U\n"), null);
  assert.equal(playlistBodyKind('<?xml version="1.0" encoding="UTF-8"?><vmap:VMAP xmlns:vmap="http://www.iab.net/videosuite/vmap" version="1.0">'), "vast");
  assert.equal(playlistBodyKind('<VAST version="4.0"><Ad/></VAST>'), "vast");
  assert.equal(playlistBodyKind("<!doctype html><html>"), "other");
  const r = new MediaRegistry();
  const vmap = "https://dmxleo.example.com/cdn/manifest/video/x1.m3u8?ps=1";
  const real = "https://cdndirector.example.com/cdn/manifest/video/x1.m3u8?sec=abc";
  r.add(1, { url: vmap, kind: "hls" });
  r.add(1, { url: real, kind: "hls" });
  r.setSignals(1, { players: [{ src: "blob:https://www.example.com/p", duration: 90, area: 600 * 340, played: 10, firstPlayAt: 1, lastPlayAt: 10000 }] });
  assert.equal(r.setNotPlaylist(1, vmap, "vast"), true);
  const l = r.list(1);
  assert.deepEqual(l.map((i) => [i.url, i.ad]), [[real, false], [vmap, true]]);
  assert.deepEqual(l[1].adReasons, ["ad-body"]);
  assert.equal(l.length, 2, "nothing is dropped");
});
