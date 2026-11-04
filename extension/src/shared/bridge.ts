// Private page-world ↔ content-script channel (opt-in page hooks only).
//
// The MAIN-world pagehook and the ISOLATED-world content script must exchange small JSON
// reports (playlist URLs, MSE counters) without the page being able to observe them. We do
// NOT use window.postMessage: a page can trivially sniff it by adding a 'message' listener or
// overriding postMessage. Instead:
//
//   * the MAIN side creates a DOM node and keeps it DETACHED (never inserted into the tree), so
//     the page has no way to obtain a reference to it and therefore cannot addEventListener on
//     it or dispatch to it;
//   * the node is handed to the ISOLATED side exactly once, at injection, through a MouseEvent
//     whose relatedTarget is the node (relatedTarget survives the MAIN→ISOLATED boundary in
//     both Chrome and Firefox). The handshake uses the native dispatchEvent captured before any
//     page script runs, with capture-phase stopImmediatePropagation, and both orderings
//     (MAIN-first or ISOLATED-first) are covered by a one-shot "poke" from the ISOLATED side;
//   * every report is then a CustomEvent on the detached node, detail = JSON string.
//
// Trade-offs (documented honestly): the handshake events themselves fire on `window` at
// document_start, before the page's own scripts can run, so the page cannot catch them after
// the fact; a page could only observe them by running code before our content scripts, which
// the browser does not allow for document_start content scripts. The report channel (the
// detached node) is unreachable from the page at any time.

export const HS_EVENT = "\u0001ldm\u0001hs";
export const HS_POKE = "\u0001ldm\u0001poke";
export const CH_EVENT = "\u0001ldm\u0001ch";

/* eslint-disable @typescript-eslint/no-explicit-any */
type Win = any;

export const ACK_EVENT = "\u0001ldm\u0001ack";

// natives captured at load (document_start, before page scripts): a page that later patches
// Function.prototype.call/apply, JSON.stringify or Object.prototype.toJSON never runs inside the
// channel and never gets a reference to the private node or a report
const apply = Reflect.apply;
const strJSON = JSON.stringify;
const keysOf = Object.keys;
const isArray = Array.isArray;

/** JSON for our own report objects (plain objects / arrays / primitives we built ourselves).
 *  Unlike JSON.stringify(obj) it never looks up `toJSON` (which a page could define on
 *  Object.prototype / Array.prototype to see our reports). */
export function toJson(v: unknown, depth = 0): string {
  if (depth > 8) return "null";
  if (v === null || v === undefined) return "null";
  const t = typeof v;
  if (t === "string") return strJSON(v); // primitives: no toJSON lookup
  if (t === "number") return v === v && v !== Infinity && v !== -Infinity ? strJSON(v) : "null";
  if (t === "boolean") return v ? "true" : "false";
  if (t !== "object") return "null";
  let out = "";
  if (isArray(v)) {
    const a = v as unknown[];
    for (let i = 0; i < a.length; i++) out += (i ? "," : "") + toJson(a[i], depth + 1);
    return "[" + out + "]";
  }
  const o = v as Record<string, unknown>;
  const ks = keysOf(o);
  let first = true;
  for (let i = 0; i < ks.length; i++) {
    const x = o[ks[i]];
    if (x === undefined || typeof x === "function") continue;
    out += (first ? "" : ",") + strJSON(ks[i]) + ":" + toJson(x, depth + 1);
    first = false;
  }
  return "{" + out + "}";
}
/** messages kept while the ISOLATED side has not acknowledged the channel yet */
export const MAX_QUEUED = 500;

/** MAIN world: returns a `post(msg)` that delivers JSON-able messages over the private channel.
 *  Messages are queued until the ISOLATED side acknowledges; after the acknowledgement the poke
 *  listener is removed, so a page script can never re-run the handshake to obtain the node. */
export function mainChannel(win: Win): (m: unknown) => void {
  try {
    const ET = win.EventTarget.prototype;
    const dispatch: (ev: unknown) => boolean = ET.dispatchEvent;
    const addEL: (...a: unknown[]) => void = ET.addEventListener;
    const removeEL: (...a: unknown[]) => void = ET.removeEventListener;
    const CE = win.CustomEvent;
    const ME = win.MouseEvent;
    const node = apply(win.Document.prototype.createElement, win.document, ["i"]); // detached; never inserted into the tree
    let queue: string[] | null = [];
    const fire = (detail: string) => {
      try {
        apply(dispatch, node, [new CE(CH_EVENT, { detail })]);
      } catch {
        /* ignore */
      }
    };
    const handshake = () => {
      try {
        apply(dispatch, win, [new ME(HS_EVENT, { relatedTarget: node })]);
      } catch {
        /* ignore */
      }
    };
    const onPoke = (e: { stopImmediatePropagation?: () => void }) => {
      try {
        e.stopImmediatePropagation?.();
      } catch {
        /* ignore */
      }
      handshake();
    };
    const onAck = () => {
      if (!queue) return;
      const q = queue;
      queue = null;
      try {
        apply(removeEL, win, [HS_POKE, onPoke, true]);
        apply(removeEL, node, [ACK_EVENT, onAck]);
      } catch {
        /* ignore */
      }
      for (let i = 0; i < q.length; i++) fire(q[i]);
    };
    apply(addEL, win, [HS_POKE, onPoke, true]);
    apply(addEL, node, [ACK_EVENT, onAck]);
    handshake();
    return (m: unknown) => {
      try {
        const d = toJson(m);
        if (queue) {
          if (queue.length < MAX_QUEUED) queue[queue.length] = d;
        } else fire(d);
      } catch {
        /* ignore */
      }
    };
  } catch {
    return () => undefined;
  }
}

/** ISOLATED world: subscribe to the private channel; `cb` receives each parsed message. */
export function isolatedChannel(win: Win, cb: (m: unknown) => void): void {
  try {
    let bound = false;
    const onHs = (e: { relatedTarget?: unknown; stopImmediatePropagation?: () => void }) => {
      try {
        e.stopImmediatePropagation?.();
      } catch {
        /* ignore */
      }
      const node = e.relatedTarget as { addEventListener?: (...a: unknown[]) => void; dispatchEvent?: (e: unknown) => boolean } | null;
      if (!node || bound || typeof node.addEventListener !== "function") return;
      bound = true;
      win.removeEventListener(HS_EVENT, onHs, true);
      node.addEventListener(CH_EVENT, (ev: { detail?: unknown }) => {
        try {
          cb(JSON.parse(String(ev.detail)));
        } catch {
          /* malformed: ignore */
        }
      });
      node.dispatchEvent?.(new CustomEvent(ACK_EVENT));
    };
    win.addEventListener(HS_EVENT, onHs, true);
    win.dispatchEvent(new Event(HS_POKE));
  } catch {
    /* ignore */
  }
}

/** Sink shared by the document_start bridge script and the (later) content script. Both run in
 *  the extension's ISOLATED world of the same frame, so this global is invisible to the page. */
export interface HookSink {
  q: unknown[];
  fn: ((m: unknown) => void) | null;
}
export const SINK_KEY = "__ldmHookSink";
export function hookSink(g: Record<string, unknown>): HookSink {
  let s = g[SINK_KEY] as HookSink | undefined;
  if (!s) g[SINK_KEY] = s = { q: [], fn: null };
  return s;
}
