// Where does the in-page "download this video" button go?
//
// Every <video>/player of every frame gets a placement score: plus points for "this is the page's
// main player" (it plays what the menu lists first, it is playing, it sits in the frame that loaded
// the main stream, it is large and visible), minus points for "this is an ad" (ad iframe / ad
// container, short, an "Ad"/"广告" label next to it, tiny or off screen). Each frame reports its own
// best player to the background, which picks ONE winner per tab, so only one button is shown per page.
// All inputs are plain data or a minimal DOM-like interface so the rules are unit-testable in node.

import { AD_DOMAINS, AD_SUBDOMAIN_LABELS, adTokenIn, domainListed, hostOfUrl, isOpaqueId, tokens, underManifest } from "./adscore";

/** Placement weights (positive = main player, negative = ad / not a usable target). */
export const PLACE_WEIGHTS = {
  /** the frame is an ad iframe (ad domain, ad word in its URL, or an ad container around the <iframe>) */
  adFrame: -6,
  /** the element or an ancestor carries an ad class/id token ("ad", "outstream", "sponsor", "preroll", …) */
  adContainer: -4,
  /** an "Ad" / "广告" / "Advertisement" label next to the player */
  adLabel: -3,
  /** known duration under PLACE_SHORT_SECS */
  short: -3,
  /** smaller than 120×80 (no room for the button either) */
  tiny: -5,
  /** not inside the viewport */
  offscreen: -4,
  /** its currentSrc / MSE streams are the menu's top (main) candidate */
  srcMatch: 4,
  /** this frame requested the menu's top candidate */
  mainFrame: 2,
  /** playing now */
  playing: 2,
  /** visible size: up to this many points at ≥ PLACE_LARGE_FRACTION of the viewport */
  large: 3,
  /** the top-level document (tie-breaker against embeds) */
  topFrame: 0.5,
} as const;

/** a known duration below this is "short" (ads are mostly 6–30 s) */
export const PLACE_SHORT_SECS = 30;
/** visible fraction of the viewport that earns the full "large" points */
export const PLACE_LARGE_FRACTION = 0.25;
/** a player in an ad frame / ad container needs a score above this to win (an outstream ad that is
 *  the only video gets no button); transient signals alone (an "Ad" label during a pre-roll in the
 *  main player, short) never remove the button from the only player */
export const PLACE_MIN_SCORE = 0;
/** the current winner is kept unless another player beats it by this much (no flapping) */
export const PLACE_HYSTERESIS = 1;
/** re-evaluation debounce in the content script (player swaps, ad end, resizes) */
export const PLACE_DEBOUNCE_MS = 300;
/** a frame's report older than this is ignored (frame gone / navigated away) */
export const PLACE_REPORT_TTL_MS = 12_000;
/** minimum size of a button target, same as policy.eligibleRect */
export const MIN_W = 120;
export const MIN_H = 80;

/** Whole class/id tokens that mark an ad container. "ad" must be a whole token: "header", "loading",
 *  "shadow", "badge" and hex ids such as "ad3f9b2c" do not match. */
export const AD_CLASS_TOKENS: ReadonlySet<string> = new Set([
  "ad", "ads", "adv", "advert", "adverts", "advertisement", "advertising", "adslot", "adunit", "adbox", "adcontainer", "adwrapper",
  "adsbygoogle", "outstream", "sponsor", "sponsored", "preroll", "midroll", "postroll", "vpaid", "ima", "commercial",
]);
/** "pre-roll", "ad-slot" style pairs */
const AD_CLASS_PAIRS = new Set(["pre roll", "mid roll", "post roll", "ad slot", "ad unit", "ad container", "ad wrapper", "out stream"]);
/** text of a nearby ad label (short texts only) */
export const AD_LABEL_RE = /^\s*(ad|ads|advertisement|sponsored|promoted|广告|赞助|推广|廣告)(\s*[·:•|-]?\s*\d{1,2}(:\d{2})?)?(\s*(of|\/)\s*\d+)?\s*$/i;

/** Class / id string → word tokens: punctuation and camelCase split, opaque ids (hex hashes) skipped. */
export function classTokens(s: string): string[] {
  return s
    .split(/[^A-Za-z0-9]+/)
    .filter((c) => c && !isOpaqueId(c))
    .flatMap((c) => c.split(/(?<=[a-z])(?=[A-Z])/))
    .flatMap((c) => tokens(c));
}

/** The ad token in a class / id string, or null. */
export function adClassToken(s: string | null | undefined): string | null {
  if (!s) return null;
  const t = classTokens(s);
  for (let i = 0; i < t.length; i++) {
    if (AD_CLASS_TOKENS.has(t[i])) return t[i];
    if (i + 1 < t.length && AD_CLASS_PAIRS.has(`${t[i]} ${t[i + 1]}`)) return `${t[i]}-${t[i + 1]}`;
  }
  return null;
}

/** Just enough of an Element for the placement rules (real DOM elements satisfy it). */
export interface ElLike {
  id?: string;
  className?: unknown;
  tagName?: string;
  parentElement: ElLike | null;
  children?: ArrayLike<ElLike>;
  textContent?: string | null;
  getAttribute?(name: string): string | null;
}

function classOf(el: ElLike): string {
  const c = el.className as unknown;
  if (typeof c === "string") return c;
  // SVG elements: SVGAnimatedString
  const b = (c as { baseVal?: unknown } | null)?.baseVal;
  return typeof b === "string" ? b : "";
}

/** Ad token on the element or one of its ancestors (`depth` levels), as "where:token". */
export function containerAdToken(el: ElLike | null, depth = 8): string | null {
  for (let i = 0; el && i <= depth; i++, el = el.parentElement) {
    const tag = (el.tagName ?? "").toLowerCase();
    if (tag === "body" || tag === "html") break;
    const tok = adClassToken(el.id) ?? adClassToken(classOf(el));
    if (tok) return `${tag || "el"}:${tok}`;
    for (const a of ["data-ad-slot", "data-ad-unit", "data-ad-client"]) if (el.getAttribute?.(a) != null) return `${tag || "el"}:${a}`;
  }
  return null;
}

/** An "Ad" / "广告" label among the short texts around the player (`up` ancestor levels, bounded walk). */
export function adLabelNear(el: ElLike | null, up = 3, maxNodes = 400, isVisible: (e: ElLike) => boolean = () => true): boolean {
  let root = el;
  for (let i = 0; i < up && root?.parentElement; i++) {
    const tag = (root.parentElement.tagName ?? "").toLowerCase();
    if (tag === "body" || tag === "html") break;
    root = root.parentElement;
  }
  if (!root) return false;
  let n = 0;
  const stack: ElLike[] = [root];
  while (stack.length && n++ < maxNodes) {
    const e = stack.pop()!;
    const kids = e.children ? Array.from(e.children) : [];
    const txt = e.textContent ?? "";
    const aria = e.getAttribute?.("aria-label") ?? "";
    if ((txt.length <= 24 && (kids.length === 0 || txt.trim().length <= 24) && AD_LABEL_RE.test(txt)) || (aria && AD_LABEL_RE.test(aria))) {
      if (isVisible(e)) return true;
      continue;
    }
    for (const k of kids) stack.push(k);
  }
  return false;
}

/** Why a frame URL is an ad frame ("ad-domain:x", "ad-subdomain:x", "ad-token:x"), or null. */
export function adFrameUrl(url: string | null | undefined, extraDomains: readonly string[] = []): string | null {
  if (!url || !/^https?:/i.test(url)) return null;
  const host = hostOfUrl(url);
  const d = domainListed(host, AD_DOMAINS) ?? domainListed(host, extraDomains);
  if (d) return `ad-domain:${d}`;
  const labels = host.split(".");
  if (labels.length > 2 && AD_SUBDOMAIN_LABELS.includes(labels[0])) return `ad-subdomain:${host}`;
  const tok = adTokenIn(url);
  return tok ? `ad-token:${tok}` : null;
}

export interface RectLike {
  top: number;
  left: number;
  width: number;
  height: number;
}

/** What the content script measured for one player. */
export interface PlayerFeatures {
  rect: RectLike;
  vw: number;
  vh: number;
  playing: boolean;
  /** media duration (s) when known and finite */
  duration?: number | null;
  /** currentSrc / MSE streams match the menu's top candidate */
  srcMatch?: boolean;
  /** this frame requested the menu's top candidate */
  mainFrame?: boolean;
  /** reason when the frame itself is an ad frame */
  adFrame?: string | null;
  /** reason when the element sits in an ad container */
  adContainer?: string | null;
  adLabel?: boolean;
  topFrame?: boolean;
}

export interface Placement {
  score: number;
  /** a button can be attached (big enough and on screen) */
  eligible: boolean;
  reasons: string[];
}

export function visibleArea(r: RectLike, vw: number, vh: number): number {
  const w = Math.max(0, Math.min(r.left + r.width, vw) - Math.max(r.left, 0));
  const h = Math.max(0, Math.min(r.top + r.height, vh) - Math.max(r.top, 0));
  return w * h;
}

export function scorePlacement(f: PlayerFeatures): Placement {
  const W = PLACE_WEIGHTS;
  let score = 0;
  const reasons: string[] = [];
  const add = (w: number, r: string) => {
    score += w;
    reasons.push(r);
  };
  const tiny = f.rect.width < MIN_W || f.rect.height < MIN_H;
  const vis = visibleArea(f.rect, f.vw, f.vh);
  const offscreen = vis <= 0;
  if (f.adFrame) add(W.adFrame, `ad-frame:${f.adFrame}`);
  if (f.adContainer) add(W.adContainer, `ad-container:${f.adContainer}`);
  if (f.adLabel) add(W.adLabel, "ad-label");
  if (f.duration != null && Number.isFinite(f.duration) && f.duration > 0 && f.duration < PLACE_SHORT_SECS) add(W.short, `short:${Math.round(f.duration)}s`);
  if (tiny) add(W.tiny, "tiny");
  if (offscreen) add(W.offscreen, "offscreen");
  if (f.srcMatch) add(W.srcMatch, "src-match");
  if (f.mainFrame) add(W.mainFrame, "main-frame");
  if (f.playing) add(W.playing, "playing");
  if (!tiny && vis > 0) {
    const frac = vis / Math.max(1, f.vw * f.vh);
    add(Math.round(W.large * Math.min(1, frac / PLACE_LARGE_FRACTION) * 100) / 100, `large:${Math.round(frac * 100)}%`);
  }
  if (f.topFrame) add(W.topFrame, "top-frame");
  return { score: Math.round(score * 100) / 100, eligible: !tiny && !offscreen, reasons };
}

/** One frame's best player, as reported to the background. */
export interface FrameReport {
  /** random per-frame token: the frame shows the button only when the background names it */
  token: string;
  score: number;
  eligible: boolean;
  reasons: string[];
  /** frame URL (for parent → child ad-iframe matching) */
  frameUrl?: string;
  /** src URLs of <iframe>s in this frame that sit in an ad container / on an ad domain */
  adIframes?: string[];
  at: number;
}

const noHash = (u: string) => u.split("#")[0];

/** Apply cross-frame knowledge: a frame whose URL the parent flagged as an ad iframe gets the ad-frame
 *  points (once). Returns adjusted copies. */
export function applyParentAdFrames(reports: FrameReport[]): FrameReport[] {
  const flagged = new Set(reports.flatMap((r) => r.adIframes ?? []).map(noHash));
  return reports.map((r) => {
    if (!r.frameUrl || !flagged.has(noHash(r.frameUrl)) || r.reasons.some((x) => x.startsWith("ad-frame"))) return r;
    return { ...r, score: Math.round((r.score + PLACE_WEIGHTS.adFrame) * 100) / 100, reasons: [...r.reasons, "ad-frame:parent-ad-container"] };
  });
}

/** The single winning frame token for a tab (or null): highest eligible score above PLACE_MIN_SCORE,
 *  the current winner kept unless beaten by PLACE_HYSTERESIS. */
export function chooseWinner(reports: FrameReport[], current: string | null = null, now = Date.now()): string | null {
  const structural = (r: FrameReport) => r.reasons.some((x) => x.startsWith("ad-frame") || x.startsWith("ad-container"));
  const live = applyParentAdFrames(reports.filter((r) => now - r.at <= PLACE_REPORT_TTL_MS)).filter((r) => r.eligible && (r.score > PLACE_MIN_SCORE || !structural(r)));
  if (!live.length) return null;
  live.sort((a, b) => b.score - a.score);
  const cur = live.find((r) => r.token === current);
  if (cur && live[0].score - cur.score < PLACE_HYSTERESIS) return cur.token;
  return live[0].token;
}

/** Is `url` (a currentSrc or a request URL) the candidate `top`? Same URL without fragment, or – for
 *  manifests – a request below the manifest's directory (its segments / variant playlists). */
export function urlMatchesCandidate(url: string, top: { url: string; kind: string; related?: string[] }): boolean {
  if (!url || !top.url) return false;
  if (noHash(url) === noHash(top.url)) return true;
  return (top.kind === "hls" || top.kind === "dash") && [top.url, ...(top.related ?? [])].some((m) => underManifest(url, m));
}
