import { test } from "node:test";
import assert from "node:assert/strict";
import { mp4Handlers, mp4Info, mp4TrackType } from "../src/shared/mp4";
import { wholeTrackTotal, WHOLE_TRACK_MIN } from "../src/shared/intercept";
import { MediaRegistry } from "../src/shared/media";

function box(type: string, ...kids: Uint8Array[]): Uint8Array {
  const len = 8 + kids.reduce((n, k) => n + k.length, 0);
  const b = new Uint8Array(len);
  new DataView(b.buffer).setUint32(0, len);
  b.set([...type].map((c) => c.charCodeAt(0)), 4);
  let o = 8;
  for (const k of kids) {
    b.set(k, o);
    o += k.length;
  }
  return b;
}
const hdlr = (h: string) => box("hdlr", new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0, ...[...h].map((c) => c.charCodeAt(0)), 0, 0, 0, 0]));
const trak = (h: string) => box("trak", box("tkhd", new Uint8Array(20)), box("mdia", box("mdhd", new Uint8Array(24)), hdlr(h)));
const cat = (...p: Uint8Array[]) => {
  const out = new Uint8Array(p.reduce((n, x) => n + x.length, 0));
  let o = 0;
  for (const x of p) {
    out.set(x, o);
    o += x.length;
  }
  return out;
};

test("MP4 header: video-only, audio-only and muxed fMP4 files; prefixes and junk are safe", () => {
  const ftyp = box("ftyp", new Uint8Array(8));
  const video = cat(ftyp, box("moov", box("mvhd", new Uint8Array(100)), trak("vide")), box("moof", new Uint8Array(40)));
  const audio = cat(ftyp, box("moov", box("mvhd", new Uint8Array(100)), trak("soun")));
  const both = cat(ftyp, box("moov", trak("vide"), trak("soun")));
  assert.equal(mp4TrackType(video), "video");
  assert.equal(mp4TrackType(audio), "audio");
  assert.equal(mp4TrackType(both), "muxed");
  assert.deepEqual(mp4Handlers(both), ["vide", "soun"]);
  // only the first bytes were fetched: the file continues past the prefix
  assert.equal(mp4TrackType(video.slice(0, video.length - 10)), "video");
  assert.equal(mp4TrackType(video.slice(0, 30)), null);
  assert.equal(mp4TrackType(new TextEncoder().encode("<html>not mp4</html>")), null);
  assert.equal(mp4TrackType(new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0])), null);
});

test("a Range-fetched .m4s with a large total is a whole track file; real segments are not", () => {
  const u = "https://upos.example.com/upgcxcode/1/2/3/clip-1-30080.m4s?e=x&deadline=1";
  assert.equal(wholeTrackTotal(u, "segment", "bytes 0-1065/12345678"), 12345678);
  assert.equal(wholeTrackTotal(u, "segment", "bytes 1066-500000/12345678"), 12345678);
  assert.equal(wholeTrackTotal(u, "segment", null), null, "DASH segment fetched whole");
  assert.equal(wholeTrackTotal(u, "segment", `bytes 0-99/${WHOLE_TRACK_MIN - 1}`), null, "small");
  assert.equal(wholeTrackTotal("https://e.com/seg-001.ts", "segment", "bytes 0-1/99999999"), null, ".ts stays a segment");
  assert.equal(wholeTrackTotal(u, "segment", "bytes 0-9/*"), null);
});

test("video-only track gets the page's audio track; labels say so", () => {
  const r = new MediaRegistry();
  const v = "https://upos.example.com/v/clip-1-30032.m4s?e=1";
  const a = "https://upos.example.com/v/clip-1-30216.m4s?e=1";
  r.add(1, { url: v, kind: "progressive", size: 8e6 });
  assert.equal(r.audioTrackFor(1), null);
  r.add(1, { url: a, kind: "progressive", size: 1e6 });
  assert.equal(r.setTrack(1, v, "video"), true);
  assert.equal(r.setTrack(1, a, "audio"), true);
  assert.equal(r.setTrack(1, a, "audio"), false, "unchanged");
  assert.equal(r.audioTrackFor(1), a);
  const l = r.list(1);
  assert.equal(l.find((i) => i.url === v)!.paired, true);
  assert.equal(l.find((i) => i.url === a)!.paired, false);
  assert.ok(l.every((i) => !i.ad), "track files on a page without manifests are not greyed");
});

test("MP4 info: movie duration (or the fragmented mehd) and the video track's size", () => {
  const u32 = (n: number) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];
  const mvhd = (ts: number, dur: number) => box("mvhd", new Uint8Array([0, 0, 0, 0, ...u32(0), ...u32(0), ...u32(ts), ...u32(dur), ...new Array(80).fill(0)]));
  const tkhd = (w: number, h: number) => box("tkhd", new Uint8Array([...new Array(76).fill(0), ...u32(w * 65536), ...u32(h * 65536)]));
  const vtrak = (w: number, h: number, kind = "vide") => box("trak", tkhd(w, h), box("mdia", hdlr(kind)));
  const moov = box("moov", mvhd(1000, 25_170), vtrak(0, 0, "soun"), vtrak(720, 1280));
  assert.deepEqual(mp4Info(cat(box("ftyp", new Uint8Array(8)), moov)), { track: "muxed", duration: 25.17, width: 720, height: 1280 });
  const frag = box("moov", mvhd(90000, 0), box("mvex", box("mehd", new Uint8Array([0, 0, 0, 0, ...u32(90000 * 12)]))), vtrak(1920, 1080));
  assert.deepEqual(mp4Info(frag), { track: "video", duration: 12, width: 1920, height: 1080 });
  assert.deepEqual(mp4Info(new Uint8Array([1, 2, 3])), { track: null, duration: null, width: null, height: null });
  assert.deepEqual(mp4Info(moov.slice(0, 60)).height, null, "a cut header is safe");
});
