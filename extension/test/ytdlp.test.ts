import { test } from "node:test";
import assert from "node:assert/strict";
import { storeExcluded, ytdlpAvailable, ytdlpPageUrl } from "../src/shared/ytdlp";
import { MediaRegistry } from "../src/shared/media";
import { qualityLabel, sortQualities } from "../src/shared/policy";

const on = { ytdlp: true, ytdlpSites: ["youtube.com", "youtu.be", "youtube-nocookie.com"], ytdlpOff: [] as string[] };

test("YouTube video pages map to one canonical URL; other pages do not", () => {
  const W = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";
  assert.equal(ytdlpPageUrl("https://www.youtube.com/watch?v=aqz-KE-bpKQ&t=42s&list=PLx", on), W);
  assert.equal(ytdlpPageUrl("https://m.youtube.com/watch?v=aqz-KE-bpKQ", on), W);
  assert.equal(ytdlpPageUrl("https://youtu.be/aqz-KE-bpKQ?si=abc", on), W);
  assert.equal(ytdlpPageUrl("https://www.youtube.com/embed/aqz-KE-bpKQ", on), W);
  assert.equal(ytdlpPageUrl("https://www.youtube.com/live/aqz-KE-bpKQ", on), W);
  assert.equal(ytdlpPageUrl("https://www.youtube.com/shorts/abcDEF12345", on), "https://www.youtube.com/shorts/abcDEF12345");
  assert.equal(ytdlpPageUrl("https://music.youtube.com/watch?v=aqz-KE-bpKQ", on), "https://music.youtube.com/watch?v=aqz-KE-bpKQ");
  for (const u of ["https://www.youtube.com/", "https://www.youtube.com/@NASA", "https://www.youtube.com/results?search_query=x", "https://www.youtube.com/watch", "https://notyoutube.com/watch?v=aqz-KE-bpKQ", "https://vimeo.com/1"])
    assert.equal(ytdlpPageUrl(u, on), null, u);
});

test("yt-dlp offered only when usable and the site is not 'off'; 'always' sites map the page", () => {
  assert.equal(ytdlpAvailable("https://vimeo.com/1", on), true);
  assert.equal(ytdlpAvailable("https://vimeo.com/1", { ...on, ytdlp: false }), false, "binary missing / disabled");
  assert.equal(ytdlpAvailable("https://www.vimeo.com/1", { ...on, ytdlpOff: ["vimeo.com"] }), false);
  assert.equal(ytdlpAvailable("chrome://newtab", on), false);
  assert.equal(ytdlpPageUrl("https://www.youtube.com/watch?v=aqz-KE-bpKQ", { ...on, ytdlpOff: ["youtube.com"], ytdlpSites: ["youtu.be"] }), null);
  const always = { ...on, ytdlpSites: [...on.ytdlpSites, "example.tv"] };
  assert.equal(ytdlpPageUrl("https://www.example.tv/v/123#c", always), "https://www.example.tv/v/123");
});

test("the yt-dlp page entry outlives the candidate TTL and is listed first", () => {
  const r = new MediaRegistry(1000);
  r.add(1, { url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ", kind: "ytdlp", title: "x" }, 0);
  r.add(1, { url: "https://cdn.example/a.mp4", kind: "progressive", size: 5_000_000 }, 0);
  const now = 5000;
  r.add(1, { url: "https://cdn.example/b.mp4", kind: "progressive", size: 5_000_000 }, now);
  const l = r.list(1, now);
  assert.deepEqual(l.map((i) => i.kind), ["ytdlp", "progressive"]);
  assert.equal(l[0].ad, false);
  assert.equal(r.remove(1, "https://www.youtube.com/watch?v=aqz-KE-bpKQ"), true);
  assert.equal(r.list(1, now).length, 1);
});

test("yt-dlp quality rows keep their own labels; audio-only last", () => {
  const qs = sortQualities([
    { label: "mp4a 129k", height: 0, bandwidth: 129000, kind: "ytdlp-audio" },
    { label: "360p · avc1", height: 360, bandwidth: 500000, kind: "ytdlp" },
    { label: "1080p60 · avc1", height: 1080, bandwidth: 4500000, kind: "ytdlp" },
  ]);
  assert.deepEqual(qs.map((q) => qualityLabel(q, "仅音频")), ["1080p60 · avc1", "360p · avc1", "仅音频 · mp4a 129k"]);
  assert.equal(qualityLabel({ label: "x", height: 720, bandwidth: 2_000_000, kind: "video" }), "720p · 2.0 Mbps", "LDM rows unchanged");
});

test("store build: no yt-dlp page rules and nothing offered on YouTube pages", () => {
  const W = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";
  assert.equal(ytdlpAvailable(W, on, true), false);
  assert.equal(ytdlpPageUrl(W, on, true), null);
  assert.equal(storeExcluded(W, true), true);
  assert.equal(storeExcluded("https://youtu.be/aqz-KE-bpKQ", true), true);
  assert.equal(storeExcluded("https://video.example/watch", true), false);
  // default build: unchanged
  assert.equal(storeExcluded(W, false), false);
  assert.equal(ytdlpPageUrl(W, on, false), W);
});
