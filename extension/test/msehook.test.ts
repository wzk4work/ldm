import { test } from "node:test";
import assert from "node:assert/strict";
import { codecLabel, codecsOf, installPageHooks, MSE_REPORT_MS, type MseReport } from "../src/shared/msehook";

/** A fake page window with native-like MediaSource / SourceBuffer / URL / HTMLMediaElement. */
function fakeWin() {
  class SourceBuffer {
    data: unknown[] = [];
    appendBuffer(d: unknown) {
      if (d == null) throw new TypeError("bad data");
      this.data.push(d);
    }
  }
  class MediaSource {
    sbs: SourceBuffer[] = [];
    addSourceBuffer(type: string) {
      if (!type.includes("/")) throw new DOMException("NotSupportedError");
      const sb = new SourceBuffer();
      this.sbs.push(sb);
      return sb;
    }
  }
  let blobN = 0;
  class URLx extends URL {
    static createObjectURL(o: unknown) {
      void o;
      return `blob:https://site.example/${++blobN}`;
    }
  }
  class HTMLMediaElement {
    _src = "";
    _obj: unknown = null;
    get src() {
      return this._src;
    }
    set src(v: string) {
      this._src = v;
    }
    get srcObject() {
      return this._obj;
    }
    set srcObject(v: unknown) {
      this._obj = v;
    }
  }
  class MediaSourceHandle {}
  const videos: HTMLMediaElement[] = [];
  const fetched: unknown[] = [];
  class XMLHttpRequest {
    opened: unknown[] = [];
    open(method: string, url: string) {
      this.opened.push([method, url]);
    }
  }
  const win = {
    Function,
    URL: URLx,
    MediaSource,
    SourceBuffer,
    HTMLMediaElement,
    MediaSourceHandle,
    XMLHttpRequest,
    location: { href: "https://site.example/watch/1" },
    document: { querySelectorAll: () => videos },
    fetch(input: unknown) {
      fetched.push(input);
      return Promise.resolve("resp");
    },
  } as Record<string, unknown>;
  return { win, videos, fetched, MediaSource, SourceBuffer, HTMLMediaElement, URLx, MediaSourceHandle, XMLHttpRequest };
}

test("codecsOf / codecLabel", () => {
  assert.deepEqual(codecsOf(['video/mp4; codecs="avc1.64001f"', 'audio/mp4; codecs="mp4a.40.2"']), ["avc1.64001f", "mp4a.40.2"]);
  assert.deepEqual(codecsOf(['video/webm; codecs="vp9, opus"', "audio/mpeg"]), ["vp9", "opus", "audio/mpeg"]);
  assert.equal(codecLabel(["avc1.64001f", "mp4a.40.2"]), "H.264+AAC");
  assert.equal(codecLabel(["av01.0.05M.08", "opus"]), "AV1+Opus");
  assert.equal(codecLabel(["hev1.1.6.L93.B0", "ec-3"]), "H.265+Dolby");
});

test("MSE hooks report codecs, byte counts, blob URL, attach and nearby manifest/segments", () => {
  const f = fakeWin();
  const msgs: Record<string, unknown>[] = [];
  let t = 1000;
  const h = installPageHooks(f.win, (m) => msgs.push(m as Record<string, unknown>), () => t);
  assert.deepEqual(h.installed.sort(), ["addSourceBuffer", "appendBuffer", "createObjectURL", "fetch", "media.src", "media.srcObject", "xhr.open"].sort());
  const fetchFn = f.win.fetch as (u: unknown) => Promise<unknown>;
  fetchFn("/hls/main.m3u8?tok=1");
  assert.deepEqual(msgs.shift(), { __ldm: "page-media", url: "https://site.example/hls/main.m3u8?tok=1" });
  const ms = new f.MediaSource();
  const blob = f.URLx.createObjectURL(ms);
  assert.equal(blob, "blob:https://site.example/1");
  const v = new f.HTMLMediaElement();
  f.videos.push(v);
  v.src = blob;
  assert.equal(v.src, blob, "setter still works");
  const vsb = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
  const asb = ms.addSourceBuffer('audio/mp4; codecs="mp4a.40.2"');
  assert.ok(vsb instanceof f.SourceBuffer, "native return value");
  new f.XMLHttpRequest().open("GET", "https://cdn.example/hls/seg-1.m4s?x=1");
  t += 100;
  const buf = new Uint8Array(1234);
  assert.equal(vsb.appendBuffer(buf), undefined);
  assert.equal(vsb.data[0], buf, "the page's own buffer is passed through untouched");
  asb.appendBuffer(new ArrayBuffer(100));
  t += MSE_REPORT_MS + 1;
  vsb.appendBuffer(new Uint8Array(10));
  const mse = msgs.filter((m) => m.__ldm === "mse") as unknown as MseReport[];
  const last = mse[mse.length - 1];
  assert.equal(last.id, 1);
  assert.deepEqual(last.blobs, [blob]);
  assert.deepEqual(last.types, ['video/mp4; codecs="avc1.64001f"', 'audio/mp4; codecs="mp4a.40.2"']);
  assert.equal(last.appends, 3);
  assert.equal(last.bytes, 1344);
  assert.equal(last.manifest, "https://site.example/hls/main.m3u8?tok=1");
  assert.deepEqual(last.segments, ["https://cdn.example/hls/seg-1.m4s"]);
  assert.ok(!JSON.stringify(msgs).includes('"0":'), "no media bytes in any report");
  assert.deepEqual(msgs.find((m) => m.__ldm === "mse-attach"), { __ldm: "mse-attach", blob, index: 0 });
  // srcObject: MediaSource and a worker MediaSourceHandle
  const v2 = new f.HTMLMediaElement();
  f.videos.push(v2);
  v2.srcObject = ms;
  assert.deepEqual(msgs[msgs.length - 1], { __ldm: "mse-attach", id: 1, index: 1 });
  v2.srcObject = new f.MediaSourceHandle();
  assert.deepEqual(msgs[msgs.length - 1], { __ldm: "mse-attach", worker: true, index: 1 });
});

test("hooks are transparent: native errors, names, lengths and descriptor flags are kept", () => {
  const f = fakeWin();
  const before = {
    add: Object.getOwnPropertyDescriptor(f.MediaSource.prototype, "addSourceBuffer")!,
    src: Object.getOwnPropertyDescriptor(f.HTMLMediaElement.prototype, "src")!,
  };
  installPageHooks(f.win, () => {
    throw new Error("posting failed"); // a broken channel must not reach the page
  });
  const add = Object.getOwnPropertyDescriptor(f.MediaSource.prototype, "addSourceBuffer")!;
  assert.notEqual(add.value, before.add.value, "hooked");
  for (const k of ["writable", "enumerable", "configurable"] as const) assert.equal(add[k], before.add[k]);
  assert.equal(add.value.name, "addSourceBuffer");
  assert.equal(add.value.length, before.add.value.length);
  const src = Object.getOwnPropertyDescriptor(f.HTMLMediaElement.prototype, "src")!;
  assert.equal(src.get, before.src.get, "getter untouched");
  assert.equal(src.set!.name, "set src");
  const ms = new f.MediaSource();
  assert.throws(() => ms.addSourceBuffer("bogus"), /NotSupportedError/, "native exception propagates");
  const sb = ms.addSourceBuffer("video/mp4");
  assert.throws(() => sb.appendBuffer(null), TypeError);
  assert.equal(sb.data.length, 0);
  // second install is a no-op
  assert.deepEqual(installPageHooks(f.win, () => undefined).installed, []);
});

test("workers / no MediaSource: nothing installed, nothing thrown", () => {
  assert.deepEqual(installPageHooks(undefined, () => undefined).installed, []);
  assert.deepEqual(installPageHooks({ fetch() {} }, () => undefined).installed, [], "no document = worker scope");
  const f = fakeWin();
  delete f.win.MediaSource;
  assert.deepEqual(installPageHooks(f.win, () => undefined).installed.sort(), ["fetch", "xhr.open"]);
});

test("progress reports: trailing flush after the throttle window; the most frequent request URL ranks first", () => {
  const f = fakeWin();
  const timers: (() => void)[] = [];
  f.win.setTimeout = (fn: () => void) => timers.push(fn);
  const msgs: MseReport[] = [];
  let t = 1000;
  installPageHooks(f.win, (m) => (m as MseReport).__ldm === "mse" && msgs.push(m as MseReport), () => t);
  const fetchFn = f.win.fetch as (u: unknown) => Promise<unknown>;
  const ms = new f.MediaSource();
  f.URLx.createObjectURL(ms);
  const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
  // one-off API calls before the first append, then the media file is range-requested before every append
  fetchFn("https://api.example/v1/list");
  fetchFn("https://api.example/v1/user");
  for (let i = 0; i < 4; i++) {
    t += 50;
    fetchFn(`https://media.example/video/abc/?range=${i}`);
    t += 50;
    sb.appendBuffer(new Uint8Array(100));
  }
  const before = msgs.length;
  assert.equal(msgs[before - 1].appends, 1, "only the first append is reported immediately");
  assert.equal(timers.length, 1, "one trailing report scheduled");
  t += MSE_REPORT_MS;
  timers[0]();
  const last = msgs[msgs.length - 1];
  assert.equal(msgs.length, before + 1);
  assert.equal(last.appends, 4);
  assert.equal(last.bytes, 400);
  assert.equal(last.segments[0], "https://media.example/video/abc/", "the media file outranks the one-off API calls");
  assert.ok(last.segments.includes("https://api.example/v1/list"));
});

test("latest: a player that switches from a low to a high bitrate file lists the high one first", () => {
  const f = fakeWin();
  f.win.setTimeout = () => 0;
  const msgs: MseReport[] = [];
  let t = 1000;
  installPageHooks(f.win, (m) => (m as MseReport).__ldm === "mse" && msgs.push(m as MseReport), () => t);
  const fetchFn = f.win.fetch as (u: unknown) => Promise<unknown>;
  const ms = new f.MediaSource();
  f.URLx.createObjectURL(ms);
  const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
  for (let i = 0; i < 6; i++) {
    t += 2000;
    fetchFn(`https://media.example/video/${i < 4 ? "low" : "high"}/?range=${i}`);
    sb.appendBuffer(new Uint8Array(10));
  }
  t += 5000;
  sb.appendBuffer(new Uint8Array(10));
  const last = msgs[msgs.length - 1];
  assert.equal(last.segments[0], "https://media.example/video/low/", "most requests went to the low file");
  assert.deepEqual(last.latest.slice(0, 2), ["https://media.example/video/high/", "https://media.example/video/low/"], "but the player is fed from the high one now");
});
