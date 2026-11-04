// Page-world observation hooks (installed by pagehook.ts in the page's MAIN world; opt-in).
//
//  * fetch / XMLHttpRequest.open: playlist URLs (.m3u8 / .mpd) are reported, and recent request
//    URLs are remembered (URL only) to link a MediaSource to its nearby manifest / segments;
//  * MediaSource.prototype.addSourceBuffer: the mime type / codecs of each SourceBuffer;
//  * SourceBuffer.prototype.appendBuffer: number of appends and their byte count only — the data
//    itself is never copied, read or stored;
//  * URL.createObjectURL(MediaSource), HTMLMediaElement src / srcObject: which <video> plays which
//    MediaSource.
//
// Every hook calls the original with the same `this` and arguments and returns its result
// unchanged (exceptions propagate unchanged). Replaced functions keep the original name, length
// and property descriptor flags so feature checks in page code keep working. The hooks are
// observe-only: nothing is modified, blocked or injected. Bookkeeping never throws.
// Where there is no window/document (workers) nothing is installed.

export const PAGE_MSG = "page-media";
export const MSE_MSG = "mse";
export const MSE_ATTACH_MSG = "mse-attach";
/** request URLs remembered for linking (most recent first is not needed; bounded ring) */
export const RECENT_REQUESTS = 40;
/** a segment request at most this long before an appendBuffer() belongs to that MediaSource */
export const SEGMENT_WINDOW_MS = 5000;
/** at most one progress report per MediaSource per this many ms */
export const MSE_REPORT_MS = 1000;
/** segment URLs reported per MediaSource (the most frequently preceding ones) */
export const MAX_SEGMENT_DIRS = 16;
/** distinct request URLs counted per MediaSource */
export const MAX_TRACKED_URLS = 64;
/** URLs reported in `latest` */
export const MAX_LATEST = 8;
export const MANIFEST_RE = /\.(m3u8|mpd)(\?|#|$)/i;

export interface MseReport {
  __ldm: typeof MSE_MSG;
  id: number;
  /** blob: URLs created for it (URL.createObjectURL) */
  blobs: string[];
  /** SourceBuffer mime types, e.g. 'video/mp4; codecs="avc1.64001f"' */
  types: string[];
  appends: number;
  bytes: number;
  /** last playlist URL requested before its first SourceBuffer (nearby manifest) */
  manifest: string | null;
  /** segment URLs (no query) requested shortly before appends, most frequent (then most recent) first */
  segments: string[];
  /** the same URLs by the append they last preceded, newest first: what the player is fed from now
   *  (a player that switched from a low to a high bitrate file lists the high one first) */
  latest: string[];
}

/** codec strings out of SourceBuffer mime types: 'video/mp4; codecs="avc1.64001f,mp4a.40.2"' → ["avc1.64001f","mp4a.40.2"] */
export function codecsOf(types: readonly string[]): string[] {
  const out: string[] = [];
  for (const t of types) {
    const m = /codecs\s*=\s*"?([^";]+)"?/i.exec(t);
    const list = m ? m[1].split(",") : [t.split(";")[0]];
    for (const c of list.map((x) => x.trim()).filter(Boolean)) if (!out.includes(c)) out.push(c);
  }
  return out;
}

/** short label: "avc1.64001f" → "H.264", … plus the raw codec ids */
export function codecLabel(codecs: readonly string[]): string {
  const name = (c: string) => {
    const k = c.toLowerCase();
    if (/^(avc1|avc3)/.test(k)) return "H.264";
    if (/^(hvc1|hev1)/.test(k)) return "H.265";
    if (/^av01/.test(k)) return "AV1";
    if (/^vp0?9/.test(k)) return "VP9";
    if (/^vp0?8/.test(k)) return "VP8";
    if (/^mp4a\.40/.test(k)) return "AAC";
    if (/^(ac-3|ec-3|mp4a\.a[56])/.test(k)) return "Dolby";
    if (/^opus/.test(k)) return "Opus";
    if (/^vorbis/.test(k)) return "Vorbis";
    if (/^flac/.test(k)) return "FLAC";
    return c;
  };
  return [...new Set(codecs.map(name))].join("+");
}

type AnyFn = (...a: unknown[]) => unknown;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Win = any;

export interface HookHandle {
  installed: string[];
}

// Natives captured when this script runs (document_start, before any page script), so later
// page patches of Function.prototype.call/apply or Object.* never run inside our wrappers.
const apply = Reflect.apply;
const gopd = Object.getOwnPropertyDescriptor;
const defProp = Object.defineProperty;
const getProto = Object.getPrototypeOf;
const wsHas = WeakSet.prototype.has;
const wsAdd = WeakSet.prototype.add;
const reExec = RegExp.prototype.exec;
const arrPush = Array.prototype.push;
const arrShift = Array.prototype.shift;
/** RegExp test through the captured native exec */
const reTest = (re: RegExp, s: string): boolean => {
  re.lastIndex = 0;
  return apply(reExec, re, [s]) !== null;
};
const HTTP_RE = /^https?:/i;
const has = (ws: WeakSet<object>, o: object) => apply(wsHas, ws, [o]) as boolean;
/** call the original the way the page called our wrapper; its exceptions propagate unchanged */
const invoke = (f: AnyFn, self: unknown, a: unknown[]): unknown => apply(f, self, a);

// Documents already hooked by this bundle instance (a WindowProxy outlives navigations, a
// document does not). A second instance in the same page is detected via HOOKED_KEY.
const hookedDocs = new WeakSet<object>();
/** non-enumerable marker on a hooked window, so a second bundle instance does not hook twice */
const HOOKED_KEY = Symbol.for("ldm.pageHooks");

/**
 * Install the hooks on `win` (the page's window). `post` receives report objects.
 * Returns which hooks were installed (for tests); an environment without a document installs none.
 */
export function installPageHooks(win: Win, post: (m: unknown) => void, now: () => number = () => Date.now()): HookHandle {
  const installed: string[] = [];
  if (!win || typeof win.document === "undefined") return { installed };
  const doc = win.document;
  if (has(hookedDocs, doc)) return { installed };
  try {
    if (win[HOOKED_KEY] === doc) return { installed };
    defProp(win, HOOKED_KEY, { value: doc, configurable: true });
  } catch {
    /* non-extensible window: rely on the WeakSet */
  }
  apply(wsAdd, hookedDocs, [doc]);
  const safe = (f: () => void) => {
    try {
      f();
    } catch {
      /* bookkeeping must never break the page */
    }
  };
  const send = (m: unknown) => safe(() => post(m));
  const shape = (w: object, orig: AnyFn) => copyShape(w, orig);

  /** replace a method keeping its descriptor flags, name and length */
  const hookMethod = (obj: object | undefined, key: string, make: (orig: AnyFn) => AnyFn, tag: string) =>
    safe(() => {
      if (!obj) return;
      const d = gopd(obj, key);
      if (!d || typeof d.value !== "function") return;
      const orig = d.value as AnyFn;
      const w = make(orig);
      shape(w, orig);
      defProp(obj, key, { ...d, value: w });
      installed.push(tag);
    });
  /** replace an accessor's setter keeping getter, flags, name ("set x") and length */
  const hookSetter = (obj: object | undefined, key: string, after: (el: unknown, v: unknown) => void, tag: string) =>
    safe(() => {
      if (!obj) return;
      const d = gopd(obj, key);
      if (!d || typeof d.set !== "function") return;
      const orig = d.set as AnyFn;
      const set = gopd(
        {
          set [key](v: unknown) {
            // eslint-disable-next-line @typescript-eslint/no-this-alias
            const el: unknown = this;
            invoke(orig, el, [v]);
            safe(() => after(el, v));
          },
        },
        key,
      )!.set as AnyFn;
      shape(set, orig);
      defProp(obj, key, { ...d, set });
      installed.push(tag);
    });

  // ---- request log (URLs only) ----
  const recent: { url: string; at: number }[] = [];
  let lastManifest: string | null = null;
  // never call into page objects (toString / getters / Proxy traps would run page code with our
  // frames on the stack): strings as they are, Request / URL objects through their native
  // (brand-checked) getters, anything else is skipped
  const URLC = win.URL;
  const getterOf = (proto: object | undefined, k: string): AnyFn | undefined => {
    try {
      return proto ? (gopd(proto, k)?.get as AnyFn | undefined) : undefined;
    } catch {
      return undefined;
    }
  };
  const reqUrl = getterOf(win.Request?.prototype, "url");
  const urlHref = getterOf(URLC?.prototype, "href");
  const rawUrl = (u: unknown): string | null => {
    if (typeof u === "string") return u;
    if (u && typeof u === "object")
      for (const g of [reqUrl, urlHref])
        if (g)
          try {
            return apply(g, u, []) as string;
          } catch {
            /* not that type */
          }
    return null;
  };
  const noted = (u: unknown) =>
    safe(() => {
      const raw = rawUrl(u);
      if (raw === null) return;
      const parsed = new URLC(raw, win.location.href);
      const url = (urlHref ? apply(urlHref, parsed, []) : parsed.href) as string;
      if (!reTest(HTTP_RE, url)) return;
      if (reTest(MANIFEST_RE, url)) {
        lastManifest = url;
        send({ __ldm: PAGE_MSG, url });
      }
      apply(arrPush, recent, [{ url, at: now() }]);
      if (recent.length > RECENT_REQUESTS) apply(arrShift, recent, []);
    });
  hookMethod(
    win,
    "fetch",
    (orig) =>
      ({
        fetch(this: unknown, ...a: unknown[]) {
          noted(a[0]);
          return invoke(orig, this, a);
        },
      }).fetch as AnyFn,
    "fetch",
  );
  hookMethod(
    win.XMLHttpRequest?.prototype,
    "open",
    (orig) =>
      ({
        open(this: unknown, ...a: unknown[]) {
          noted(a[1]);
          return invoke(orig, this, a);
        },
      }).open as AnyFn,
    "xhr.open",
  );

  // ---- MediaSource observation ----
  const MS = win.MediaSource;
  const SB = win.SourceBuffer;
  if (typeof MS !== "function") return { installed };
  const msBrand = getterOf(MS.prototype, "readyState");
  const isMS = (o: unknown): boolean => {
    if (!o || typeof o !== "object") return false;
    if (!msBrand) return o instanceof MS;
    try {
      apply(msBrand, o, []);
      return true;
    } catch {
      return false;
    }
  };
  const lenGetters = [
    getterOf((win.ArrayBuffer ?? ArrayBuffer).prototype, "byteLength"),
    getterOf((win.DataView ?? DataView).prototype, "byteLength"),
    getterOf(getProto((win.Uint8Array ?? Uint8Array).prototype), "byteLength"),
  ];
  const byteLen = (d: unknown): number => {
    if (!d || typeof d !== "object") return 0;
    for (const g of lenGetters)
      if (g)
        try {
          return Number(apply(g, d, [])) || 0;
        } catch {
          /* other type */
        }
    return 0;
  };
  interface St {
    id: number;
    blobs: string[];
    types: string[];
    appends: number;
    bytes: number;
    manifest: string | null;
    /** request URL (no query) → number of appends it preceded; the media file / segment directory wins over one-off API calls */
    hits: Map<string, { n: number; at: number; last: number }>;
    lastAppend: number;
    lastPost: number;
    timer: unknown;
  }
  let nextId = 1;
  const states = new WeakMap<object, St>();
  const ofBuffer = new WeakMap<object, St>();
  const stateOf = (ms: object): St => {
    let s = states.get(ms);
    if (!s) states.set(ms, (s = { id: nextId++, blobs: [], types: [], appends: 0, bytes: 0, manifest: null, hits: new Map(), lastAppend: 0, lastPost: 0, timer: null }));
    return s;
  };
  const setTimer: AnyFn | undefined = typeof win.setTimeout === "function" ? win.setTimeout.bind(win) : undefined;
  const segmentsOf = (s: St) =>
    [...s.hits]
      .sort((a, b) => b[1].n - a[1].n || b[1].at - a[1].at)
      .slice(0, MAX_SEGMENT_DIRS)
      .map(([u]) => u);
  const latestOf = (s: St) =>
    [...s.hits]
      .sort((a, b) => b[1].last - a[1].last || b[1].n - a[1].n || b[1].at - a[1].at)
      .slice(0, MAX_LATEST)
      .map(([u]) => u);
  const flush = (s: St) => {
    s.timer = null;
    s.lastPost = now();
    const m: MseReport = { __ldm: MSE_MSG, id: s.id, blobs: [...s.blobs], types: [...s.types], appends: s.appends, bytes: s.bytes, manifest: s.manifest, segments: segmentsOf(s), latest: latestOf(s) };
    send(m);
  };
  /** report now (forced or not throttled), else once at the end of the throttle window (trailing) */
  const report = (s: St, force: boolean) => {
    const wait = MSE_REPORT_MS - (now() - s.lastPost);
    if (force || wait <= 0) return flush(s);
    if (!s.timer && setTimer) s.timer = setTimer(() => safe(() => flush(s)), wait);
  };
  hookMethod(
    MS.prototype,
    "addSourceBuffer",
    (orig) =>
      ({
        addSourceBuffer(this: object, ...a: unknown[]) {
          const sb = invoke(orig, this, a);
          safe(() => {
            const s = stateOf(this);
            const type = typeof a[0] === "string" ? a[0].slice(0, 200) : "";
            if (type && !s.types.includes(type) && s.types.length < 8) s.types.push(type);
            s.manifest ??= lastManifest;
            if (sb && typeof sb === "object") ofBuffer.set(sb as object, s);
            report(s, true);
          });
          return sb;
        },
      }).addSourceBuffer as AnyFn,
    "addSourceBuffer",
  );
  hookMethod(
    SB?.prototype,
    "appendBuffer",
    (orig) =>
      ({
        appendBuffer(this: object, ...a: unknown[]) {
          const r = invoke(orig, this, a);
          safe(() => {
            const s = ofBuffer.get(this);
            if (!s) return;
            const first = s.appends === 0;
            s.appends++;
            s.bytes += byteLen(a[0]);
            // requests since the previous append (within the window) are counted once each
            const t = now();
            const seen = new Set<string>();
            for (const q of recent) {
              if (t - q.at > SEGMENT_WINDOW_MS || q.at < s.lastAppend || MANIFEST_RE.test(q.url)) continue;
              const u = q.url.split(/[?#]/)[0];
              if (seen.has(u)) continue;
              seen.add(u);
              const h = s.hits.get(u);
              if (h) Object.assign(h, { n: h.n + 1, at: q.at, last: s.appends });
              else if (s.hits.size < MAX_TRACKED_URLS) s.hits.set(u, { n: 1, at: q.at, last: s.appends });
            }
            s.lastAppend = t;
            report(s, first);
          });
          return r;
        },
      }).appendBuffer as AnyFn,
    "appendBuffer",
  );
  hookMethod(
    win.URL,
    "createObjectURL",
    (orig) =>
      ({
        createObjectURL(this: unknown, ...a: unknown[]) {
          const url = invoke(orig, this, a);
          safe(() => {
            if (typeof url === "string" && isMS(a[0])) {
              const s = stateOf(a[0] as object);
              if (s.blobs.length < 4) s.blobs.push(url);
              report(s, true);
            }
          });
          return url;
        },
      }).createObjectURL as AnyFn,
    "createObjectURL",
  );
  const media = win.HTMLMediaElement?.prototype;
  const qsa: AnyFn = doc.querySelectorAll;
  const indexOf = (el: unknown) => {
    const list = apply(qsa, doc, ["video,audio"]) as ArrayLike<unknown>;
    for (let i = 0; i < list.length; i++) if (list[i] === el) return i;
    return -1;
  };
  hookSetter(
    media,
    "src",
    (el, v) => {
      if (typeof v === "string" && v.startsWith("blob:")) send({ __ldm: MSE_ATTACH_MSG, blob: v, index: indexOf(el) });
    },
    "media.src",
  );
  hookSetter(
    media,
    "srcObject",
    (el, v) => {
      if (isMS(v)) send({ __ldm: MSE_ATTACH_MSG, id: stateOf(v as object).id, index: indexOf(el) });
      else if (win.MediaSourceHandle && v instanceof win.MediaSourceHandle) send({ __ldm: MSE_ATTACH_MSG, worker: true, index: indexOf(el) });
    },
    "media.srcObject",
  );
  return { installed };
}

/** give `w` the native's name and length (both non-writable, configurable like natives) */
// eslint-disable-next-line @typescript-eslint/no-unsafe-function-type
function copyShape(w: object, orig: Function): void {
  defProp(w, "name", { value: orig.name, configurable: true });
  defProp(w, "length", { value: orig.length, configurable: true });
}
