import { test } from "node:test";
import assert from "node:assert/strict";
import { classify, dispositionFilename, mediaKind, urlExt } from "../src/shared/classify";
import { DEFAULT_RULES } from "../src/shared/protocol";

test("urlExt ignores query and fragment", () => {
  assert.equal(urlExt("https://h/a/b/video.MP4?x=1#t"), "mp4");
  assert.equal(urlExt("https://h/dir/"), "");
  assert.equal(urlExt("not a url/file.zip?q"), "zip");
});

test("classify table", () => {
  const cases: [string, string | null, string][] = [
    ["https://h/master.m3u8", null, "manifest"],
    ["https://h/x?y", "application/vnd.apple.mpegurl", "manifest"],
    ["https://h/manifest.mpd", null, "manifest"],
    ["https://h/p", "application/dash+xml; charset=utf-8", "manifest"],
    ["https://h/seg001.ts", null, "segment"],
    ["https://h/chunk-1.m4s", null, "segment"],
    ["https://h/v.mp4", null, "media"],
    ["https://h/stream", "video/webm", "media"],
    ["https://h/a.zip", null, "file"],
    ["https://h/page.html", "text/html", "ignore"],
  ];
  for (const [u, m, want] of cases) assert.equal(classify(u, m, null, DEFAULT_RULES), want, u);
  assert.equal(classify("https://h/dl?id=3", "application/octet-stream", 'attachment; filename="x.bin"'), "file");
});

test("mediaKind", () => {
  assert.equal(mediaKind("https://h/a.m3u8"), "hls");
  assert.equal(mediaKind("https://h/a.mpd"), "dash");
  assert.equal(mediaKind("https://h/a.mp4"), "progressive");
});

test("content-disposition", () => {
  assert.equal(dispositionFilename('attachment; filename="a b.zip"'), "a b.zip");
  assert.equal(dispositionFilename("attachment; filename*=UTF-8''%E8%A7%86%E9%A2%91.mp4"), "视频.mp4");
  assert.equal(dispositionFilename("inline"), null);
});
