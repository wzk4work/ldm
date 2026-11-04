import { test } from "node:test";
import assert from "node:assert/strict";
import { codecsFor, menuEntries, mseLinks, rankOrder, scoreAll, underManifest, type PageSignals, type RankInput } from "../src/shared/adscore";
import { MediaRegistry } from "../src/shared/media";
import { urlMatchesCandidate } from "../src/shared/placement";

const MAIN = "https://cdn.site.example/v/123/master.m3u8";
const AD = "https://cdn.other.example/creative/abc/index.m3u8";

test("MSE links: manifest, folded variant, segments under the manifest directory", () => {
  assert.ok(underManifest("https://cdn.site.example/v/123/720/seg1.m4s", MAIN));
  assert.ok(!underManifest("https://cdn.site.example/v/999/seg1.m4s", MAIN));
  assert.ok(mseLinks({ manifest: MAIN }, { url: MAIN }));
  assert.ok(mseLinks({ manifest: "https://cdn.site.example/v/123/720/index.m3u8" }, { url: MAIN }), "variant under master dir");
  assert.ok(mseLinks({ manifest: "https://x.example/p/720.m3u8" }, { url: MAIN, related: ["https://x.example/p/720.m3u8"] }), "folded variant elsewhere");
  assert.ok(mseLinks({ segments: ["https://cdn.site.example/v/123/a/1.ts"] }, { url: MAIN }));
  assert.ok(!mseLinks({ segments: ["https://cdn.other.example/creative/abc/1.ts"] }, { url: MAIN }));
  assert.ok(!mseLinks(null, { url: MAIN }));
  assert.ok(urlMatchesCandidate("https://x.example/p/seg9.ts", { url: MAIN, kind: "hls", related: ["https://x.example/p/720.m3u8"] }));
});

const twoStreams = (mainPlaying: boolean): { items: RankInput[]; page: PageSignals } => ({
  items: [
    // the ad stream was seen last and is listed first by recency
    { url: AD, kind: "hls", firstSeen: 2000 },
    { url: MAIN, kind: "hls", firstSeen: 1000 },
  ],
  page: {
    players: [
      // both are blob: MSE players; the ad one is bigger but small-played; the main one is playing
      { src: "blob:https://site.example/a", area: 640 * 360, played: 30, playing: false, mse: { manifest: AD, codecs: ["avc1.42e01e", "mp4a.40.2"] } },
      { src: "blob:https://site.example/m", area: 480 * 270, played: 5, playing: mainPlaying, mse: { manifest: MAIN, segments: ["https://cdn.site.example/v/123/720/s1.m4s"], codecs: ["avc1.64001f", "mp4a.40.2"] } },
    ],
  },
});

test("a playing MSE player's stream counts as main (mse-main), links keep players apart", () => {
  const { items, page } = twoStreams(true);
  const v = scoreAll(items, { page });
  assert.ok(v[1].reasons.includes("mse-main"), v[1].reasons.join());
  assert.ok(!v[0].reasons.includes("mse-main"));
  // without links every blob player maps to every manifest: main-player went to both before
  assert.ok(v[0].reasons.includes("main-player") && !v[1].reasons.includes("main-player"));
  assert.deepEqual(rankOrder(v), [1, 0], "playing MSE stream first despite the bigger/longer ad player");
  assert.deepEqual(codecsFor(items[1], page), ["avc1.64001f", "mp4a.40.2"]);
  assert.deepEqual(codecsFor(items[0], page), ["avc1.42e01e", "mp4a.40.2"]);
  // paused → no mse-main
  const p = twoStreams(false);
  assert.ok(!scoreAll(p.items, { page: p.page })[1].reasons.includes("mse-main"));
  // an ad-context MSE player never makes its stream "main"
  const q = twoStreams(true);
  q.page.players![1].adContext = "ad-frame:x";
  assert.ok(!scoreAll(q.items, { page: q.page })[1].reasons.includes("mse-main"));
});

test("unlinked blob players still map to manifests (hook off / worker MSE): old behaviour", () => {
  const items: RankInput[] = [{ url: MAIN, kind: "hls" }];
  const page: PageSignals = { players: [{ src: "blob:https://site.example/m", area: 640 * 360, played: 20, playing: true }] };
  const v = scoreAll(items, { page });
  assert.ok(v[0].reasons.includes("main-player"));
  assert.ok(!v[0].reasons.includes("mse-main"));
  const w: PageSignals = { players: [{ src: "blob:https://site.example/m", area: 640 * 360, played: 20, playing: true, mse: { worker: true, codecs: [] } }] };
  assert.ok(scoreAll(items, { page: w })[0].reasons.includes("main-player"));
});

test("registry: codecs attached to the linked manifest (via a folded variant), tooltip shows them", () => {
  const r = new MediaRegistry();
  const now = Date.now();
  r.add(1, { url: "https://a.example/hls/master.m3u8", kind: "hls" }, now);
  r.add(1, { url: "https://b.example/v720/index.m3u8", kind: "hls" }, now + 1);
  r.setChildren(1, "https://a.example/hls/master.m3u8", ["https://b.example/v720/index.m3u8"]);
  const sig0 = r.rankSignature(1);
  r.setSignals(1, { players: [{ src: "blob:https://a.example/1", area: 1e5, played: 3, playing: true, mse: { manifest: "https://b.example/v720/index.m3u8", codecs: ["avc1.4d401f", "mp4a.40.2"] } }] });
  const l = r.list(1);
  assert.equal(l.length, 1);
  assert.deepEqual(l[0].codecs, ["avc1.4d401f", "mp4a.40.2"]);
  assert.ok(l[0].adReasons!.includes("mse-main"));
  assert.notEqual(r.rankSignature(1), sig0, "codecs change pushes the button rows");
  assert.match(menuEntries(l)[0].tooltip, /codecs: avc1\.4d401f, mp4a\.40\.2/);
});

test("progressive files fed through MSE (range requests): exact file link, mse-main and codecs", () => {
  const FILE = "https://media.site.example/video/tos/abc/?a=1&sig=x";
  const TEASER = "https://static.site.example/webapp/playback1.mp4";
  const items: RankInput[] = [
    { url: TEASER, kind: "progressive", size: 200_000, firstSeen: 1000 },
    { url: FILE, kind: "progressive", size: 3_200_000, firstSeen: 1500 },
  ];
  const page: PageSignals = {
    players: [
      {
        src: "blob:https://www.site.example/1",
        area: 439 * 780,
        played: 20,
        playing: true,
        duration: 25,
        mse: { segments: ["https://media.site.example/video/tos/abc/", "https://api.site.example/v1/list"], codecs: ["mp4a.40.29", "avc1.64001f"] },
      },
    ],
  };
  assert.ok(mseLinks(page.players![0].mse, items[1]), "same file, query ignored");
  assert.ok(!mseLinks(page.players![0].mse, items[0]));
  assert.ok(!mseLinks({ segments: ["https://media.site.example/video/tos/"] }, items[1]), "a parent directory is not the file");
  const v = scoreAll(items, { page });
  assert.ok(v[1].reasons.includes("mse-main") && v[1].reasons.includes("main-player"), v[1].reasons.join());
  assert.ok(!v[0].reasons.includes("mse-main"));
  assert.deepEqual(rankOrder(v), [1, 0]);
  assert.deepEqual(codecsFor(items[1], page), ["mp4a.40.29", "avc1.64001f"]);
  assert.deepEqual(codecsFor(items[0], page), []);
});
