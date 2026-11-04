// "Main video or ad?" ranking for detected media candidates.
//
// Every candidate gets a score: positive points say "looks like an ad", negative points say
// "looks like the page's main video". The list is ordered main-first; candidates at or above
// AD_THRESHOLD are moved to the bottom and shown greyed with a "疑似广告/Ad?" tag. Nothing is
// ever hidden or dropped — a greyed entry stays clickable and downloadable.
//
// The weights are plain constants so they are easy to tune.

/** Weights per signal (positive = ad-ish, negative = main-ish). */
export const AD_WEIGHTS = {
  /** host is on the built-in ad / ad-tech domain list */
  adDomain: 3.0,
  /** host is on the user's ad domain list (media.ad_domains_file) */
  userAdDomain: 3.0,
  /** host's first label names an ad server ("ads.", "adserver.", …) */
  adSubdomain: 2.0,
  /** URL path / file name contains an ad word (ad, ads, vast, preroll, creative, promo, …) */
  adToken: 2.0,
  /** a progressive file while the page also streams an HLS/DASH manifest */
  progressiveBesideManifest: 1.25,
  /** host differs from every manifest host on the page */
  otherHost: 0.75,
  /** duration under SHORT_SECS */
  short: 1.5,
  /** duration clearly differs from the page-declared duration */
  durationMismatch: 1.0,
  /** duration within DURATION_MATCH_SECS of the page-declared duration */
  durationMatch: -3.0,
  /** seen before the main stream and stopped playing within SHORT_SECS */
  earlyShort: 1.0,
  /** what the largest on-screen player played longest */
  mainPlayer: -2.0,
  /** its player was playing while a "Skip Ad" style control was visible */
  skipControl: 2.0,
  /** its player sits in an ad iframe / ad container / next to an "Ad" label */
  adContext: 2.0,
  /** first seen while a "Skip Ad" control was visible (no player mapping) */
  skipWindow: 1.0,
  /** first seen shortly after a VAST / VMAP / IMA ad request from the tab */
  adRequest: 1.0,
  /** manifests are usually the real stream */
  manifest: -0.5,
  /** a playing MediaSource player (not in ad context) is fed from this manifest (MSE hook) */
  mseMain: -1.5,
  /** a ".m3u8"/".mpd" URL whose body is a VAST/VMAP ad description (seen on the real web) */
  adBody: 6.0,
  /** a playlist URL whose body is not a playlist at all (HTML/JSON error page …) */
  notPlaylist: 3.0,
} as const;

/** Score at or above which a candidate is greyed and moved to the bottom. */
export const AD_THRESHOLD = 2.5;
export const SHORT_SECS = 35;
export const DURATION_MATCH_SECS = 3;
/** a candidate first seen within this time after an ad request / before a skip control */
export const AD_WINDOW_MS = 20_000;
/** progressive files above this size do not get the "beside a manifest" point */
export const SMALL_PROGRESSIVE_BYTES = 30 * 1024 * 1024;

/** Ad / ad-tech domains (our own short list; subdomains match too). Extend it with media.ad_domains_file. */
export const AD_DOMAINS: readonly string[] = [
  "doubleclick.net", "googlesyndication.com", "googleadservices.com", "googletagservices.com", "2mdn.net",
  "imasdk.googleapis.com", "adnxs.com", "adsrvr.org", "rubiconproject.com", "pubmatic.com", "openx.net",
  "criteo.com", "taboola.com", "outbrain.com", "amazon-adsystem.com", "moatads.com", "adform.net",
  "smartadserver.com", "spotxchange.com", "springserve.com", "teads.tv", "innovid.com", "serving-sys.com",
  "trafficjunky.net", "trafficjunky.com", "exoclick.com", "exosrv.com", "juicyads.com", "adtng.com",
  "trafficstars.com", "tsyndicate.com", "popads.net", "propellerads.com", "adsterra.com", "adspyglass.com",
  "smilewanted.com", "richaudience.com",
];

/** First host labels that name an ad server on a site's own domain. */
export const AD_SUBDOMAIN_LABELS: readonly string[] = ["ad", "ads", "adserver", "adservice", "adserving", "adimg", "adcdn", "banners", "vast"];

/** Whole-word URL tokens that mark ad media ("download", "head", "upload" do not match). */
const AD_TOKENS = new Set([
  "ad", "ads", "adv", "advert", "adverts", "advertising", "vast", "vpaid", "vmap", "preroll", "prerolls",
  "midroll", "postroll", "creative", "creatives", "promo", "promos", "sponsor", "sponsored", "commercial", "commercials",
]);
/** two-word forms such as "pre-roll", "pre_roll" */
const AD_TOKEN_PAIRS = new Set(["pre roll", "mid roll", "post roll", "ad break", "ad slot"]);

/** VAST / VMAP / IMA ad requests (checked on every request URL of a tab). */
export function isAdRequestUrl(url: string): boolean {
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    return false;
  }
  const host = u.hostname.toLowerCase();
  if (host === "imasdk.googleapis.com" || host === "pubads.g.doubleclick.net") return true;
  const toks = tokens(u.pathname + " " + [...u.searchParams.keys()].join(" "));
  return toks.some((t) => t === "vast" || t === "vmap" || t === "vpaid" || t === "ima3");
}

/** Lower-case word tokens of a string; letter/digit runs are split ("ads123" → ads, 123). */
export function tokens(s: string): string[] {
  return s
    .toLowerCase()
    .split(/[^a-z0-9]+|(?<=[a-z])(?=[0-9])|(?<=[0-9])(?=[a-z])/)
    .filter(Boolean);
}

/** A path chunk that is an id, not words: hex with a digit (≥ 6 chars), or a long letter+digit mix. */
export function isOpaqueId(chunk: string): boolean {
  const c = chunk.toLowerCase();
  const mixed = /\d/.test(c) && /[a-z]/.test(c);
  return (mixed && c.length >= 6 && /^[0-9a-f]+$/.test(c)) || (mixed && c.length >= 12);
}

/** URL ad words: in the path and file name only (query values are often tracking noise). */
export function adTokenIn(url: string): string | null {
  let path = url;
  try {
    path = decodeURIComponent(new URL(url).pathname);
  } catch {
    /* keep as is */
  }
  // opaque ids ("ccfaa6de", "badb6ad1", long base64-ish runs) are not words: "6ad1" is no "ad"
  const t = path
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((c) => c && !isOpaqueId(c))
    .flatMap((c) => tokens(c));
  for (let i = 0; i < t.length; i++) {
    if (AD_TOKENS.has(t[i])) return t[i];
    if (i + 1 < t.length && AD_TOKEN_PAIRS.has(`${t[i]} ${t[i + 1]}`)) return `${t[i]}-${t[i + 1]}`;
  }
  return null;
}

export function hostOfUrl(url: string | null | undefined): string {
  try {
    return url ? new URL(url).hostname.toLowerCase() : "";
  } catch {
    return "";
  }
}

export function domainListed(host: string, list: readonly string[]): string | null {
  for (const d of list) {
    const x = d.trim().toLowerCase().replace(/^\*?\./, "");
    if (x && (host === x || host.endsWith("." + x))) return x;
  }
  return null;
}

/** Parse a user ad-domain list: one domain per line, `#` comments, hosts-file lines ("0.0.0.0 ads.example"). */
export function parseDomainList(text: string): string[] {
  const out: string[] = [];
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.replace(/#.*/, "").trim();
    if (!line) continue;
    const parts = line.split(/\s+/);
    const d = (parts.length > 1 && /^[\d.:]+$/.test(parts[0]) ? parts[1] : parts[0]).toLowerCase().replace(/^\*?\./, "");
    if (/^[a-z0-9.-]+\.[a-z0-9-]+$/.test(d) && d !== "localhost") out.push(d);
  }
  return out;
}

/** One <video>/<audio> playback record from the content script (keyed by element + source). */
export interface PlayerRecord {
  /** currentSrc (blob: for MSE players) */
  src: string;
  /** media duration in seconds, when finite */
  duration?: number | null;
  /** largest visible area seen, px² */
  area: number;
  /** seconds actually played */
  played: number;
  firstPlayAt?: number | null;
  lastPlayAt?: number | null;
  /** a "Skip Ad" style control was visible while it played */
  duringSkip?: boolean;
  /** playing at the last sample */
  playing?: boolean;
  /** Media Source Extensions observation (page hook, opt-in): what this blob: player is fed with */
  mse?: MseLink | null;
  /** it played inside an ad iframe / ad container / next to an "Ad" label (shared/placement.ts), as a reason */
  adContext?: string | null;
  /** id of the <video>/<audio> element (one element may play several sources over time) */
  el?: string;
}

/** What the MSE hook saw for one MediaSource: SourceBuffer codecs, nearby manifest / segment URLs. */
export interface MseLink {
  codecs?: string[];
  manifest?: string | null;
  segments?: string[];
  /** request URLs by the append they last preceded, newest first (what the player is fed from now) */
  latest?: string[];
  bytes?: number;
  appends?: number;
  /** the MediaSource lives in a worker (MediaSourceHandle): no codecs/links visible */
  worker?: boolean;
}

export interface PageSignals {
  /** duration declared by the page (og:video:duration, itemprop=duration, JSON-LD) */
  declaredDuration?: number | null;
  players?: PlayerRecord[];
  /** [start, end] times (ms) a "Skip Ad" control was visible */
  skipWindows?: [number, number][];
  /** times (ms) of VAST / VMAP / IMA requests from the tab */
  adRequests?: number[];
}

export interface RankInput {
  url: string;
  kind: "progressive" | "hls" | "dash" | "ytdlp";
  size?: number | null;
  firstSeen?: number;
  /** duration known from the playlist (manifests) */
  duration?: number | null;
  /** set when fetching the "playlist" showed it is not one: "vast" (VAST/VMAP ad XML) or "other" */
  notPlaylist?: "vast" | "other" | null;
  /** variant / rendition playlists folded into this master (they link MSE players to it too) */
  related?: string[];
  /** single-track file type from its MP4 header (video-only and audio-only files are never alternates) */
  track?: "video" | "audio" | "muxed" | null;
  /** video height in pixels, when known (MP4 header) */
  height?: number | null;
}

export interface AdVerdict {
  score: number;
  ad: boolean;
  /** "code" or "code:detail", see describeReason */
  reasons: string[];
}

export interface RankOptions {
  enabled?: boolean;
  userDomains?: readonly string[];
  builtinDomains?: readonly string[];
  page?: PageSignals | null;
}

const isBlob = (s: string) => s.startsWith("blob:");
const sameUrl = (a: string, b: string) => a.split("#")[0] === b.split("#")[0];
const samePath = (a: string, b: string) => !!a && a.split(/[?#]/)[0] === b.split(/[?#]/)[0];

/** The player record that is the page's main video: biggest area × time played, not during a skip control. */
function mainRecord(page?: PageSignals | null): PlayerRecord | null {
  let best: PlayerRecord | null = null;
  for (const p of page?.players ?? []) {
    if (p.duringSkip || p.adContext || p.played < 1 || p.area < 120 * 80) continue;
    if (!best || p.area * p.played > best.area * best.played) best = p;
  }
  return best;
}

/** Is `url` the manifest itself, or a file under its directory on the same host (segments)? */
export function underManifest(url: string, manifest: string): boolean {
  if (!url || !manifest) return false;
  if (sameUrl(url, manifest)) return true;
  try {
    const a = new URL(manifest);
    const b = new URL(url);
    const dir = a.pathname.replace(/[^/]*$/, "");
    return a.host === b.host && dir.length > 1 && b.pathname.startsWith(dir);
  } catch {
    return false;
  }
}

/**
 * Does this MSE observation point at candidate `c`? Manifests: its manifest, a folded variant, or
 * segments under them. Progressive files fed through MSE (range requests appended by the player):
 * the same file (query ignored) among the requests preceding the appends.
 */
export function mseLinks(m: MseLink | null | undefined, c: { url: string; related?: string[]; kind?: string }): boolean {
  if (!m) return false;
  const urls = [c.url, ...(c.related ?? [])];
  if (c.kind === "progressive") return (m.segments ?? []).some((s) => urls.some((u) => samePath(s, u)));
  if (m.manifest && urls.some((u) => sameUrl(m.manifest!, u) || underManifest(m.manifest!, u))) return true;
  return (m.segments ?? []).some((s) => urls.some((u) => underManifest(s, u)));
}

const hasLinks = (m: MseLink | null | undefined) => !!m && (!!m.manifest || !!m.segments?.length);

/**
 * Records belonging to a candidate: its own URL; for manifests the blob: (MSE) players linked to it
 * by the MSE hook, else the blob: players not linked to another candidate of the page.
 */
function recordsFor(c: RankInput, page?: PageSignals | null, all: RankInput[] = []): PlayerRecord[] {
  const ps = page?.players ?? [];
  if (c.kind === "progressive") return ps.filter((p) => sameUrl(p.src, c.url) || (isBlob(p.src) && mseLinks(p.mse, c)));
  const blobs = ps.filter((p) => isBlob(p.src));
  const linked = blobs.filter((p) => mseLinks(p.mse, c));
  if (linked.length) return linked;
  const others = all.filter((o) => o !== c);
  return blobs.filter((p) => !hasLinks(p.mse) || !others.some((o) => mseLinks(p.mse, o)));
}

/** Codecs of the MediaSource players fed from this manifest (MSE hook), e.g. ["avc1.64001f", "mp4a.40.2"]. */
export function codecsFor(c: RankInput, page?: PageSignals | null): string[] {
  const out: string[] = [];
  for (const p of page?.players ?? []) if (isBlob(p.src) && mseLinks(p.mse, c)) for (const k of p.mse?.codecs ?? []) if (!out.includes(k)) out.push(k);
  return out;
}

/** Score every candidate of one page. */
export function scoreAll(items: RankInput[], opts: RankOptions = {}): AdVerdict[] {
  if (opts.enabled === false) return items.map(() => ({ score: 0, ad: false, reasons: [] }));
  const W = AD_WEIGHTS;
  const page = opts.page ?? null;
  const builtin = opts.builtinDomains ?? AD_DOMAINS;
  const manifests = items.filter((i) => i.kind !== "progressive");
  const manifestHosts = new Set(manifests.map((m) => hostOfUrl(m.url)));
  const main = mainRecord(page);
  // page duration: declared, else the main player's own duration
  const declared = page?.declaredDuration && page.declaredDuration > 0 ? page.declaredDuration : null;
  const pageDur = declared ?? (main?.duration && Number.isFinite(main.duration) && main.duration > 0 ? main.duration : null);
  const firstManifest = manifests.reduce((t, m) => Math.min(t, m.firstSeen ?? Infinity), Infinity);
  return items.map((c) => {
    let score = 0;
    const reasons: string[] = [];
    const add = (w: number, r: string) => {
      score += w;
      reasons.push(r);
    };
    const host = hostOfUrl(c.url);
    const user = domainListed(host, opts.userDomains ?? []);
    const known = domainListed(host, builtin);
    if (user) add(W.userAdDomain, `user-ad-domain:${user}`);
    else if (known) add(W.adDomain, `ad-domain:${known}`);
    else if (AD_SUBDOMAIN_LABELS.includes(host.split(".")[0]) && host.split(".").length > 2) add(W.adSubdomain, `ad-subdomain:${host}`);
    const tok = adTokenIn(c.url);
    if (tok) add(W.adToken, `ad-token:${tok}`);
    if (c.notPlaylist === "vast") add(W.adBody, "ad-body");
    else if (c.notPlaylist) add(W.notPlaylist, "not-a-playlist");
    if (c.kind !== "progressive" && !c.notPlaylist) add(W.manifest, "manifest");
    if (c.kind === "progressive" && manifests.length) {
      if (c.size == null || c.size < SMALL_PROGRESSIVE_BYTES) add(W.progressiveBesideManifest, "progressive-beside-manifest");
      if (host && !manifestHosts.has(host)) add(W.otherHost, `other-host:${host}`);
    }
    // a non-playlist is not what any player is showing
    const recs = c.notPlaylist ? [] : recordsFor(c, page, items);
    // duration: the playlist's, else its player's
    const dur = c.duration ?? recs.map((r) => r.duration).find((d) => d != null && Number.isFinite(d) && d > 0) ?? null;
    if (dur != null) {
      if (pageDur != null && Math.abs(dur - pageDur) <= DURATION_MATCH_SECS) add(W.durationMatch, `duration-match:${Math.round(dur)}s`);
      else {
        if (dur < SHORT_SECS) add(W.short, `short:${Math.round(dur)}s`);
        if (pageDur != null && Math.abs(dur - pageDur) > Math.max(10, pageDur * 0.1)) add(W.durationMismatch, `duration-mismatch:${Math.round(dur)}s/${Math.round(pageDur)}s`);
      }
    }
    // timing: appeared before the main stream and stopped quickly
    const r0 = recs[0];
    if (
      c.kind === "progressive" &&
      c.firstSeen != null && c.firstSeen < firstManifest &&
      r0?.firstPlayAt != null && r0.lastPlayAt != null && r0.lastPlayAt - r0.firstPlayAt < SHORT_SECS * 1000 &&
      main && !sameUrl(main.src, c.url) && !recs.includes(main)
    )
      add(W.earlyShort, "early-short");
    if (main && recs.includes(main)) add(W.mainPlayer, "main-player");
    // MSE hook: a playing MediaSource player outside any ad context is fed from this manifest / file
    if (recs.some((r) => r.playing && !r.adContext && !r.duringSkip && isBlob(r.src) && mseLinks(r.mse, c))) add(W.mseMain, "mse-main");
    // player signals: a visible "Skip Ad" control, VAST/VMAP/IMA requests
    // (the main player's own stream is often requested while the pre-roll's "Skip Ad" is still up)
    const isMain = !!main && recs.includes(main);
    if (recs.some((r) => r.duringSkip) && !(c.kind !== "progressive" && isMain)) add(W.skipControl, "skip-control");
    // a manifest maps to every blob: player of the page, so only when all of them are in ad context
    const ctx = isMain ? null : c.kind === "progressive" ? recs.find((r) => r.adContext)?.adContext : recs.length && recs.every((r) => r.adContext) ? recs[0].adContext : null;
    if (ctx) add(W.adContext, `ad-context:${ctx}`);
    else if (!isMain && c.firstSeen != null && (page?.skipWindows ?? []).some(([s, e]) => c.firstSeen! >= s - AD_WINDOW_MS && c.firstSeen! <= e)) add(W.skipWindow, "skip-window");
    if (!isMain && c.firstSeen != null && (page?.adRequests ?? []).some((t) => c.firstSeen! >= t && c.firstSeen! - t <= AD_WINDOW_MS)) add(W.adRequest, "ad-request");
    score = Math.round(score * 100) / 100;
    return { score, ad: score >= AD_THRESHOLD, reasons };
  });
}

/** Two durations (seconds) of the same content: within 1 s or 2 %. */
const sameLength = (a?: number | null, b?: number | null) => a == null || b == null || !(a > 0) || !(b > 0) || Math.abs(a - b) <= Math.max(1, 0.02 * Math.max(a, b));

/**
 * Quality alternates: progressive files that are the same video at different bitrates. Two
 * non-ad files are the same video when the same <video> element played both (one after the other
 * through its src / currentSrc, or both fed into its MediaSource), they hold the same track type
 * and their durations (when known) agree. In each group the main entry is what the element ended
 * up using — the file its MediaSource was fed from last (MSE hook `latest`), else the src it
 * played last — and without such evidence the higher resolution, then the higher bitrate, then
 * the bigger file. Returns, per item, the index of its group's main entry (null for main entries
 * and ungrouped items). Suspected ads are never grouped (their greying stays as it is).
 */
export function qualityAlternates(items: RankInput[], verdicts: AdVerdict[], page?: PageSignals | null): (number | null)[] {
  const out: (number | null)[] = items.map(() => null);
  const players = page?.players ?? [];
  const playerKey = (p: PlayerRecord) => (p.el ? `el:${p.el}` : `src:${p.src}`);
  const linked = items.map((c, i) => {
    const keys = new Set<string>();
    if (c.kind !== "progressive" || verdicts[i]?.ad || c.notPlaylist) return keys;
    for (const p of players) if (!p.adContext && (sameUrl(p.src, c.url) || (isBlob(p.src) && mseLinks(p.mse, c)))) keys.add(playerKey(p));
    // page hook off: a blob player without MSE details, and a file whose own MP4 header length is the player's
    if (!keys.size && c.duration != null && c.duration > 0)
      for (const p of players)
        if (!p.adContext && isBlob(p.src) && !p.mse && p.duration != null && p.duration > 0 && sameLength(p.duration, c.duration) && (p.played ?? 0) > 0) keys.add(playerKey(p));
    return keys;
  });
  const compatible = (a: number, b: number) =>
    (items[a].track ?? null) === (items[b].track ?? null) && items[a].track !== "audio" && sameLength(items[a].duration, items[b].duration);
  // greedy grouping in rank order (best-scored first)
  const order = rankOrder(verdicts);
  const groups: number[][] = [];
  for (const i of order) {
    if (!linked[i].size) continue;
    const g = groups.find((g) => g.some((j) => [...linked[j]].some((k) => linked[i].has(k))) && g.every((j) => compatible(i, j)));
    if (g) g.push(i);
    else groups.push([i]);
  }
  for (const g of groups) {
    if (g.length < 2) continue;
    const main = pickCurrent(g, items, players) ?? pickBest(g, items, order);
    for (const j of g) if (j !== main) out[j] = main;
  }
  return out;
}

/** The member the player is using now: first in an MSE player's `latest`, else the src played last. */
function pickCurrent(g: number[], items: RankInput[], players: PlayerRecord[]): number | null {
  let best: { i: number; idx: number; at: number } | null = null;
  for (const p of players) {
    const latest = isBlob(p.src) ? p.mse?.latest ?? [] : [];
    for (const i of g) {
      const idx = latest.findIndex((u) => samePath(u, items[i].url));
      const at = p.lastPlayAt ?? p.firstPlayAt ?? 0;
      if (idx >= 0 && (!best || at > best.at || (at === best.at && idx < best.idx))) best = { i, idx, at };
    }
  }
  if (best) return best.i;
  let direct: { i: number; at: number } | null = null;
  for (const p of players)
    for (const i of g) {
      const at = p.lastPlayAt ?? p.firstPlayAt ?? 0;
      if (sameUrl(p.src, items[i].url) && (!direct || at > direct.at)) direct = { i, at };
    }
  return direct?.i ?? null;
}

/** Without playback evidence: higher resolution, higher bitrate (bytes per second), bigger file, better rank. */
function pickBest(g: number[], items: RankInput[], order: number[]): number {
  const rate = (c: RankInput) => (c.size && c.duration && c.duration > 0 ? c.size / c.duration : 0);
  return [...g].sort(
    (a, b) =>
      (items[b].height ?? 0) - (items[a].height ?? 0) ||
      rate(items[b]) - rate(items[a]) ||
      (items[b].size ?? 0) - (items[a].size ?? 0) ||
      order.indexOf(a) - order.indexOf(b),
  )[0];
}

/** Rank order with each quality group kept together: its main entry where the group's best member
 *  ranked, its alternates right after it (in their own rank order). */
export function groupedOrder(order: number[], alt: (number | null)[]): number[] {
  const out: number[] = [];
  const done = new Set<number>();
  for (const i of order) {
    const main = alt[i] ?? i;
    if (done.has(main)) continue;
    done.add(main);
    out.push(main);
    for (const j of order) if (alt[j] === main) out.push(j);
  }
  return out;
}

/** Order: likely main video first (lowest score), suspected ads last; ties keep the input order. */
export function rankOrder(verdicts: AdVerdict[]): number[] {
  return verdicts
    .map((v, i) => ({ v, i }))
    .sort((a, b) => Number(a.v.ad) - Number(b.v.ad) || a.v.score - b.v.score || a.i - b.i)
    .map((x) => x.i);
}

const TEXT: Record<string, [string, string]> = {
  "ad-domain": ["广告域名", "ad domain"],
  "ad-body": ["内容是 VAST/VMAP 广告描述，不是视频列表", "body is a VAST/VMAP ad description, not a playlist"],
  "not-a-playlist": ["内容不是播放列表", "body is not a playlist"],
  "user-ad-domain": ["自定义广告域名", "user ad domain"],
  "ad-subdomain": ["广告子域名", "ad subdomain"],
  "ad-token": ["URL 含广告词", "ad word in URL"],
  "progressive-beside-manifest": ["页面另有 HLS/DASH 流", "small file beside a stream"],
  "other-host": ["与主视频流不同主机", "different host than the stream"],
  short: ["时长很短", "short"],
  "duration-mismatch": ["时长与页面不符", "duration differs from page"],
  "duration-match": ["时长与页面一致", "duration matches page"],
  "early-short": ["先于正片出现且很快结束", "played briefly before the main video"],
  "main-player": ["主播放器长时间播放", "main player played it longest"],
  "skip-control": ["播放时有“跳过广告”按钮", "played while 'Skip Ad' was shown"],
  "ad-context": ["在广告框架/广告容器中播放", "played in an ad frame / ad container"],
  "skip-window": ["出现在“跳过广告”期间", "seen while 'Skip Ad' was shown"],
  "ad-request": ["紧随 VAST/IMA 广告请求", "right after a VAST/IMA ad request"],
  manifest: ["流媒体清单", "stream manifest"],
  "mse-main": ["正在播放的 MSE 播放器使用此流", "a playing MSE player is fed from it"],
  "alt-of": ["同一视频的其他码率", "another bitrate of the same video"],
};

/** Human text for one reason code ("zh" or "en"). */
export function describeReason(r: string, lang: "zh" | "en" = "en"): string {
  const i = r.indexOf(":");
  const code = i < 0 ? r : r.slice(0, i);
  let detail = i < 0 ? "" : r.slice(i + 1);
  // "alt-of:<url>": the main entry's file name
  if (code === "alt-of") detail = detail.split(/[?#]/)[0].split("/").filter(Boolean).pop() ?? detail;
  const t = TEXT[code]?.[lang === "zh" ? 0 : 1] ?? code;
  return detail ? `${t} (${detail})` : t;
}

/** Tooltip text: score + reasons. */
export function describeVerdict(v: { score?: number; reasons?: string[] }, lang: "zh" | "en" = "en"): string {
  const head = lang === "zh" ? `广告评分 ${v.score ?? 0}` : `ad score ${v.score ?? 0}`;
  return [head, ...(v.reasons ?? []).map((r) => "· " + describeReason(r, lang))].join("\n");
}

/** ISO-8601 duration ("PT1M5S", "P0DT2H") or plain seconds → seconds. */
export function parseDuration(v: string | null | undefined): number | null {
  if (!v) return null;
  const s = v.trim();
  if (/^\d+(\.\d+)?$/.test(s)) return Number(s) || null;
  const m = /^P(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+(?:\.\d+)?)S)?)?$/i.exec(s);
  if (!m) return null;
  const secs = Number(m[1] ?? 0) * 86400 + Number(m[2] ?? 0) * 3600 + Number(m[3] ?? 0) * 60 + Number(m[4] ?? 0);
  return secs > 0 ? secs : null;
}

/** Text of a visible "Skip Ad" style control (EN/ZH), tested on short element texts / aria-labels. */
export const SKIP_CONTROL_RE = /^\s*(skip(\s+(the\s+)?ads?)?|skip\s+in\s+\d+|skip\s+ad\s+in\s+\d+|跳过(广告)?|关闭广告|广告\s*\d+|ad\s*\d+\s*(of|\/)\s*\d+|ad\s*[·:]\s*\d+(:\d+)?|advertisement)\s*[›»>▶]?\s*$/i;

export interface MenuItemIn {
  url: string;
  kind: string;
  ad?: boolean;
  adScore?: number | null;
  adReasons?: string[];
  codecs?: string[];
  /** URL of the main entry when this is another bitrate of the same video */
  altOf?: string | null;
}

export interface MenuEntry<T> {
  item: T;
  greyed: boolean;
  /** another bitrate of the entry above: shown indented under it */
  sub: boolean;
  /** "疑似广告" / "Ad?" or null */
  tag: string | null;
  /** score + reasons, for the title attribute */
  tooltip: string;
}

/** In-page menu / popup rows: keep the background's ranking, ads (already last) greyed with a tag. */
export function menuEntries<T extends MenuItemIn>(items: T[], lang: "zh" | "en" = "en"): MenuEntry<T>[] {
  const main = items.filter((i) => !i.ad);
  const ads = items.filter((i) => i.ad);
  return [...main, ...ads].map((item) => ({
    item,
    greyed: !!item.ad,
    sub: !item.ad && !!item.altOf && main.some((m) => m.url === item.altOf),
    tag: item.ad ? (lang === "zh" ? "疑似广告" : "Ad?") : !item.ad && item.altOf ? (lang === "zh" ? "其他码率" : "Alt. quality") : null,
    tooltip: [item.url, item.codecs?.length ? `codecs: ${item.codecs.join(", ")}` : "", item.adReasons?.length ? describeVerdict({ score: item.adScore ?? 0, reasons: item.adReasons }, lang) : ""].filter(Boolean).join("\n"),
  }));
}

/** One-click shortcut of the in-page button: download straight away only when the single candidate is a
 *  plain file that is not ad-like. An ad-like one opens the menu instead, so the user sees the "Ad?" tag
 *  before choosing it (it is still offered, never dropped). */
export function directDownloadItem<T extends MenuItemIn>(items: T[]): T | null {
  return items.length === 1 && items[0].kind === "progressive" && !items[0].ad ? items[0] : null;
}

/** Ad verdict of an incoming item: the flat `ad: boolean` form, or a nested `{ad, score, reasons}` verdict
 *  (older daemons). Anything else is "not an ad" — an unknown shape must never grey a row. */
export function adFields(it: { ad?: unknown; adScore?: number | null; adReasons?: string[] }): { ad: boolean; adScore: number | null; adReasons: string[] } {
  const v = it.ad;
  if (v && typeof v === "object") {
    const o = v as { ad?: unknown; score?: unknown; reasons?: unknown };
    return { ad: o.ad === true, adScore: typeof o.score === "number" ? o.score : null, adReasons: Array.isArray(o.reasons) ? o.reasons.map(String) : [] };
  }
  return { ad: v === true, adScore: it.adScore ?? null, adReasons: Array.isArray(it.adReasons) ? it.adReasons : [] };
}

export function uiLang(): "zh" | "en" {
  try {
    const l = (typeof chrome !== "undefined" && chrome.i18n?.getUILanguage?.()) || (typeof navigator !== "undefined" ? navigator.language : "en");
    return /^zh/i.test(l) ? "zh" : "en";
  } catch {
    return "en";
  }
}
