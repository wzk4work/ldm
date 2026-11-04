// Content script: media element tracking, in-page shadow-DOM "download this video" button with a
// quality list, link-click modifier tracking, link collection.
import { eligibleRect, qualityLabel, sortQualities, type Mods, type Quality } from "./shared/policy";
import { mp4Info } from "./shared/mp4";
import { adFields, directDownloadItem, menuEntries, mseLinks, parseDuration, SKIP_CONTROL_RE, uiLang, type MseLink, type PlayerRecord } from "./shared/adscore";
import { codecLabel, codecsOf, MAX_LATEST, MAX_SEGMENT_DIRS, MSE_ATTACH_MSG, MSE_MSG, PAGE_MSG, type MseReport } from "./shared/msehook";
import { hookSink } from "./shared/bridge";
import {
  adFrameUrl, adLabelNear, containerAdToken, PLACE_DEBOUNCE_MS, scorePlacement, urlMatchesCandidate, type ElLike, type Placement, type PlayerFeatures,
} from "./shared/placement";

interface Item {
  url: string;
  kind: string;
  size?: number | null;
  at: number;
  /** ad ranking from the background (suspected ads are greyed, never hidden) */
  ad?: boolean;
  track?: "video" | "audio" | "muxed" | null;
  paired?: boolean;
  adScore?: number | null;
  adReasons?: string[];
  /** codecs of the MSE player fed from it (page hook) */
  codecs?: string[];
  related?: string[];
  /** another bitrate of the same video: the main entry's URL */
  altOf?: string | null;
}

const ELEMENT_ATTR = "data-ldm-id";
const FRESH_MS = 120_000;
let seq = 0;
let host: HTMLElement | null = null;
let shadow: ShadowRoot | null = null;
let items: Item[] = [];
let selectors: string[] = [];
let target: Element | null = null;
let dismissed = false;
/** yt-dlp usable for this page: the menu always ends with "Download with yt-dlp". */
let ytdlpOk = false;
const t = (k: string, d: string) => (typeof chrome !== "undefined" && chrome.i18n?.getMessage(k)) || d;

function ensureId(el: Element): string {
  let id = el.getAttribute(ELEMENT_ATTR);
  if (!id) {
    id = `ldm-${++seq}`;
    el.setAttribute(ELEMENT_ATTR, id);
  }
  return id;
}

// ---- button placement (shared/placement.ts): score every player of this frame, report the best
// one to the background, show the button only when the background names this frame the winner ----
const frameToken = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
const isTop = window === window.top;
/** winner token from the background (null: no frame of this tab may show the button) */
let winner: string | null = null;
let localBest: Element | null = null;
/** URLs this frame requested (resource timing): "the frame that loaded the main stream" */
const frameRequests: string[] = [];
try {
  new PerformanceObserver((l) => {
    for (const e of l.getEntries()) {
      frameRequests.push(e.name);
      if (frameRequests.length > 600) frameRequests.splice(0, 200);
    }
  }).observe({ type: "resource", buffered: true });
} catch {
  /* no resource timing */
}

function frameAdReason(): string | null {
  if (isTop) return null;
  const u = adFrameUrl(location.href);
  if (u) return u;
  try {
    // same-origin frames can see their own <iframe> element and its containers
    return containerAdToken(window.frameElement as unknown as ElLike | null);
  } catch {
    return null;
  }
}
let frameAd: string | null = frameAdReason();

const visibleEl = (e: ElLike) => {
  const el = e as unknown as HTMLElement;
  if (!el.getClientRects || !el.getClientRects().length) return false;
  return getComputedStyle(el).visibility !== "hidden";
};

/** The player's media element: itself, or the <video> inside a site-adapter container. */
function mediaOf(el: Element): HTMLMediaElement | null {
  return el instanceof HTMLMediaElement ? el : el.querySelector("video");
}

function topCandidate(): Item | null {
  return freshItems().find((i) => !i.ad && i.kind !== "ytdlp") ?? null;
}

function playerCandidates(): Element[] {
  const cands: Element[] = [...document.querySelectorAll("video, audio")];
  for (const sel of selectors) {
    try {
      cands.push(...document.querySelectorAll(sel));
    } catch {
      /* invalid selector from a rules file */
    }
  }
  return cands;
}

function featuresOf(el: Element, top: Item | null, mainFrame: boolean): PlayerFeatures {
  const r = el.getBoundingClientRect();
  const m = mediaOf(el);
  const src = m?.currentSrc ?? "";
  const playing = !!m && !m.paused && !m.ended && m.readyState >= 2;
  // an MSE player linked by the page hook to its manifest/segments matches exactly; otherwise a
  // playing blob: player in the main frame is assumed to play the top stream
  const mse = m ? mseOf(m) : null;
  const linked = mse && (mse.manifest || mse.segments?.length);
  const srcMatch =
    !!top && !!src && (urlMatchesCandidate(src, top) || (linked ? mseLinks(mse, top) : (src.startsWith("blob:") || !!mse) && mainFrame && playing));
  return {
    rect: { top: r.top, left: r.left, width: r.width, height: r.height },
    vw: innerWidth,
    vh: innerHeight,
    playing,
    duration: m && Number.isFinite(m.duration) ? m.duration : null,
    srcMatch,
    mainFrame,
    adFrame: frameAd,
    adContainer: containerAdToken(el as unknown as ElLike),
    adLabel: adLabelNear(el as unknown as ElLike, 3, 400, visibleEl),
    topFrame: isTop,
  };
}

/** Score this frame's players; returns the best one and its placement. */
function evaluatePlayers(): { el: Element; p: Placement } | null {
  const top = topCandidate();
  const mainFrame = !!top && frameRequests.some((u) => urlMatchesCandidate(u, top));
  let best: { el: Element; p: Placement } | null = null;
  for (const el of playerCandidates()) {
    const p = scorePlacement(featuresOf(el, top, mainFrame));
    if (!best || Number(p.eligible) - Number(best.p.eligible) > 0 || (p.eligible === best.p.eligible && p.score > best.p.score)) best = { el, p };
  }
  return best;
}

/** <iframe>s of this frame that are ad frames (ad container around them / ad domain): their own
 *  content script cannot see the container, so the background applies this to the child frame. */
function adIframes(): string[] {
  const out: string[] = [];
  for (const f of document.querySelectorAll<HTMLIFrameElement>("iframe[src]")) {
    if (out.length >= 20) break;
    if (/^https?:/i.test(f.src) && (containerAdToken(f as unknown as ElLike) || adFrameUrl(f.src))) out.push(f.src);
  }
  return out;
}

let lastReport = "";
let lastReportAt = 0;
let evalTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleEvaluate(): void {
  if (evalTimer) return;
  evalTimer = setTimeout(() => {
    evalTimer = null;
    evaluateAndReport();
  }, PLACE_DEBOUNCE_MS);
}

function evaluateAndReport(): void {
  if (!freshItems().length) {
    localBest = null;
    return hide();
  }
  const best = evaluatePlayers();
  localBest = best?.p.eligible ? best.el : null;
  const ifr = adIframes();
  const report = best || ifr.length ? { token: frameToken, score: best?.p.score ?? -99, eligible: !!best?.p.eligible, reasons: best?.p.reasons ?? [], frameUrl: location.href, adIframes: ifr } : null;
  const key = JSON.stringify(report && { ...report, reasons: undefined });
  const now = Date.now();
  if (key !== lastReport || now - lastReportAt > 5000) {
    lastReport = key;
    lastReportAt = now;
    chrome.runtime
      .sendMessage({ type: "PlacementReport", report })
      .then((r: { winner?: string | null } | undefined) => {
        if (r && "winner" in r) setWinner(r.winner ?? null);
      })
      .catch(() => undefined);
  }
  // re-render (rebuilds the menu) only when the button moves to another player; otherwise just
  // re-position, so an open menu is not rebuilt under the user's pointer every evaluation
  if ((winner === frameToken ? localBest : null) !== target || !host || host.style.display === "none") render();
  else place();
}

function setWinner(w: string | null): void {
  if (w === winner) return;
  winner = w;
  render();
}

function label(it: Item): string {
  // last non-empty path segment ("…/okRjOk…/?a=1" → "okRjOk…"), shortened
  const seg = it.url.split(/[?#]/)[0].split("/").filter(Boolean).pop() ?? it.url;
  let name = seg;
  try {
    name = decodeURIComponent(seg);
  } catch {
    /* keep */
  }
  if (name.length > 48) name = name.slice(0, 45) + "…";
  const zh = uiLang() === "zh";
  const track =
    it.track === "video" ? (it.paired ? (zh ? " [视频+音频]" : " [video+audio]") : zh ? " [仅视频]" : " [video only]") : it.track === "audio" ? (zh ? " [音频]" : " [audio]") : "";
  if (it.kind === "ytdlp") return zh ? "此页视频 [yt-dlp]" : "This page's video [yt-dlp]";
  const kind = (it.kind === "progressive" ? "" : ` [${it.kind.toUpperCase()}]`) + track;
  // a manifest's own byte size says nothing about the video
  const size = it.kind === "progressive" && it.size ? ` (${(it.size / 1048576).toFixed(1)} MB)` : "";
  // codecs the page's MSE player was fed (page hook), e.g. " · H.264+AAC"
  const codecs = it.codecs?.length ? ` · ${codecLabel(it.codecs)}` : "";
  return `${name}${kind}${codecs}${size}`;
}

function ensureButton(): ShadowRoot {
  if (shadow && host?.isConnected) return shadow;
  host = document.createElement("ldm-download-button");
  host.style.cssText = "position:fixed;z-index:2147483647;top:0;left:0;display:none;";
  shadow = host.attachShadow({ mode: "closed" });
  shadow.innerHTML = `
    <style>
      :host { all: initial; }
      .btn { font: 13px/1.2 system-ui, sans-serif; background: #1a73e8; color: #fff; border: 0; border-radius: 4px;
             padding: 6px 10px; cursor: pointer; box-shadow: 0 1px 4px rgba(0,0,0,.4); opacity: .92; }
      .btn:hover { opacity: 1; }
      .menu { display: none; margin-top: 4px; background: #fff; color: #202124; border-radius: 4px; min-width: 240px;
              max-width: 420px; max-height: 360px; overflow: auto; box-shadow: 0 2px 10px rgba(0,0,0,.35); font: 13px system-ui, sans-serif; }
      .menu.open { display: block; }
      .item { padding: 7px 10px; cursor: pointer; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
      .item:hover { background: #e8f0fe; }
      .q { padding-left: 22px; color: #1a73e8; }
      .alt { padding-left: 22px; color: #3c4043; }
      .item.adish { color: #9aa0a6; }
      .adtag { display: inline-block; margin-right: 6px; padding: 0 4px; border: 1px solid #bdc1c6; border-radius: 3px; font-size: 11px; color: #80868b; }
      .hint { padding: 6px 10px; color: #5f6368; font-size: 12px; }
      .close { float: right; margin-left: 6px; cursor: pointer; color: #fff; }
    </style>
    <button class="btn" part="button"><span class="text"></span><span class="close" title="×">×</span></button>
    <div class="menu"></div>`;
  const btn = shadow.querySelector(".btn") as HTMLButtonElement;
  const menu = shadow.querySelector(".menu") as HTMLDivElement;
  (shadow.querySelector(".text") as HTMLSpanElement).textContent = t("buttonDownloadVideo", "Download with LDM");
  btn.addEventListener("click", (ev) => {
    if ((ev.target as HTMLElement).classList.contains("close")) {
      dismissed = true;
      host!.style.display = "none";
      return;
    }
    const one = directDownloadItem(freshItems());
    if (one) download(one);
    else menu.classList.toggle("open");
  });
  (document.documentElement || document.body).appendChild(host);
  return shadow;
}

/** og:title and the first visible h1: the video title on sites whose <title> is just the site name. */
function pageTitles(): { og: string | null; h1: string | null } {
  const og = document.querySelector<HTMLMetaElement>('meta[property="og:title"], meta[name="twitter:title"]')?.content?.trim() || null;
  const h = Array.from(document.querySelectorAll<HTMLElement>("h1")).find((e) => e.offsetParent !== null && e.innerText.trim());
  return { og, h1: h ? h.innerText.trim().slice(0, 400) : null };
}

function download(it: Item, height?: number): void {
  chrome.runtime.sendMessage({ type: "UserDownloadMedia", url: it.url, height: height ?? null, titles: pageTitles() }).catch(() => undefined);
  shadow?.querySelector(".menu")?.classList.remove("open");
}

function freshItems(now = Date.now()): Item[] {
  return items.filter((i) => i.kind === "ytdlp" || now - i.at < FRESH_MS);
}

/** Probe results per URL: the menu is rebuilt on every ranking update (e.g. while a pre-roll plays),
 *  so an open quality list is re-attached from here instead of being lost or probed again. */
const probed = new Map<string, "pending" | { qs: Quality[]; error?: string }>();
let expandedUrl: string | null = null;

/** Manifest / yt-dlp entries expand into their quality list (fetched from LDM via MediaProbe). */
async function expand(it: Item, row: HTMLDivElement): Promise<void> {
  expandedUrl = it.url;
  const prev = probed.get(it.url);
  if (prev && (prev === "pending" || !prev.error)) return showQualities(it, row); // errors may be retried
  probed.set(it.url, "pending");
  showQualities(it, row);
  const r = await chrome.runtime.sendMessage({ type: "ProbeMedia", url: it.url, kind: it.kind === "ytdlp" ? "ytdlp" : undefined }).catch(() => null);
  probed.set(it.url, { qs: r?.ok ? sortQualities(r.qualities ?? []) : [], error: r?.ok ? undefined : r?.error ? String(r.error) : undefined });
  // the menu may have been rebuilt meanwhile: use this entry's current row
  const cur = Array.from(shadow?.querySelectorAll<HTMLDivElement>(".item[data-url]") ?? []).find((d) => d.dataset.url === it.url);
  if (cur && expandedUrl === it.url) showQualities(it, cur);
}

function showQualities(it: Item, row: HTMLDivElement): void {
  const menu = row.parentElement;
  menu?.querySelectorAll(".q, .hint").forEach((e) => e.remove());
  const st = probed.get(it.url);
  let after: Element = row;
  const line = (cls: string, text: string, h?: number | null) => {
    const d = document.createElement("div");
    d.className = cls;
    d.textContent = text;
    if (h !== null) d.addEventListener("click", () => download(it, h));
    after.after(d);
    after = d;
  };
  if (!st || st === "pending") return line("hint", t("qualityLoading", "…"), null);
  if (st.error) line("hint", st.error.slice(0, 300), null);
  line("item q", t("qualityBest", "Best quality"));
  const seen = new Set<string>();
  for (const q of st.qs) {
    const text = qualityLabel(q, t("qualityAudioOnly", "Audio only"));
    if (seen.has(text)) continue;
    seen.add(text);
    line("item q", text, q.kind === "audio" || q.kind === "ytdlp-audio" ? 0 : q.height ?? undefined);
  }
}

function render(): void {
  const fresh = freshItems();
  if (!fresh.length || dismissed) return hide();
  target = winner === frameToken ? localBest : null;
  if (!target || !target.isConnected || document.fullscreenElement) return hide();
  const root = ensureButton();
  const menu = root.querySelector(".menu") as HTMLDivElement;
  menu.textContent = "";
  for (const row of menuEntries(fresh, uiLang())) {
    const it = row.item;
    const d = document.createElement("div");
    d.className = row.greyed ? "item adish" : row.sub ? "item alt" : "item";
    if (row.tag) {
      const tag = document.createElement("span");
      tag.className = "adtag";
      tag.textContent = row.tag;
      d.appendChild(tag);
    }
    d.appendChild(document.createTextNode(label(it)));
    d.title = row.tooltip;
    d.dataset.url = it.url;
    d.addEventListener("click", () => (it.kind === "progressive" ? download(it) : void expand(it, d)));
    menu.appendChild(d);
    if (it.url === expandedUrl && it.kind !== "progressive") showQualities(it, d);
  }
  // always offered: hand the page to the user's yt-dlp instead of what LDM sniffed
  if (ytdlpOk && !fresh.some((i) => i.kind === "ytdlp")) {
    const d = document.createElement("div");
    d.className = "item";
    d.textContent = t("menuDownloadYtdlp", "Download with yt-dlp");
    d.addEventListener("click", () => {
      chrome.runtime.sendMessage({ type: "UserDownloadYtdlp", url: location.href, titles: pageTitles() }).catch(() => undefined);
      menu.classList.remove("open");
    });
    menu.appendChild(d);
  }
  place();
  host!.style.display = "block";
}

function hide(): void {
  if (host) host.style.display = "none";
}

function place(): void {
  if (!host) return;
  const r = target?.getBoundingClientRect();
  if (!r || !eligibleRect(r, innerWidth, innerHeight)) {
    hide();
    return;
  }
  host.style.top = `${Math.max(4, r.top + 8)}px`;
  host.style.left = `${Math.max(4, Math.min(innerWidth - 200, r.right - 190))}px`;
  if (!dismissed && !document.fullscreenElement && freshItems().length) host.style.display = "block";
}

function reportGeometry(): void {
  for (const el of document.querySelectorAll("video, audio")) {
    const r = el.getBoundingClientRect();
    chrome.runtime
      .sendMessage({ type: "MediaGeometry", payload: { element_id: ensureId(el), rect: { x: r.x, y: r.y, w: r.width, h: r.height, dpr: devicePixelRatio }, visible: r.width > 0 && r.height > 0 } })
      .catch(() => undefined);
  }
}

function collectLinks(): { url: string; text: string }[] {
  const out = new Map<string, string>();
  for (const a of document.querySelectorAll<HTMLAnchorElement | HTMLAreaElement>("a[href], area[href]")) {
    if (/^https?:/i.test(a.href) && !out.has(a.href)) out.set(a.href, (a.textContent || "").trim().slice(0, 120));
  }
  for (const m of document.querySelectorAll<HTMLMediaElement | HTMLSourceElement>("video[src], audio[src], source[src]")) {
    if (/^https?:/i.test(m.src) && !out.has(m.src)) out.set(m.src, "");
  }
  return [...out].map(([url, text]) => ({ url, text }));
}

// ---- modifier keys on link clicks (Alt: browser keeps it; Insert: force LDM; configurable in LDM 选项)
let insertHeld = false;
addEventListener("keydown", (e) => {
  if (e.key === "Insert") insertHeld = true;
}, true);
addEventListener("keyup", (e) => {
  if (e.key === "Insert") insertHeld = false;
}, true);
addEventListener("blur", () => (insertHeld = false));

function onLinkPress(e: MouseEvent): void {
  if (e.button > 1) return;
  const a = (e.target as Element | null)?.closest?.("a[href], area[href]") as HTMLAnchorElement | null;
  if (!a || !/^https?:/i.test(a.href)) return;
  const mods: Mods = { alt: e.altKey, shift: e.shiftKey, ctrl: e.ctrlKey || e.metaKey, insert: insertHeld };
  chrome.runtime.sendMessage({ type: "LinkClick", url: a.href, mods }).catch(() => undefined);
}
addEventListener("mousedown", onLinkPress, true);

function merge(
  incoming: {
    url: string; kind: string; size?: number | null; lastSeen?: number; ad?: unknown; adScore?: number | null; adReasons?: string[]; track?: Item["track"]; paired?: boolean;
    codecs?: string[]; related?: string[]; altOf?: string | null;
  }[],
  sel?: string[],
): void {
  const now = Date.now();
  selectors = sel ?? selectors;
  // the background always sends the tab's full current list: drop entries it no longer lists
  // (e.g. a variant playlist folded into its HLS master)
  const keep = new Set(incoming.map((i) => i.url));
  for (let i = items.length - 1; i >= 0; i--) if (!keep.has(items[i].url)) items.splice(i, 1);
  for (const it of incoming) {
    const at = Math.min(now, it.lastSeen ?? now);
    const old = items.find((i) => i.url === it.url);
    const ad = adFields(it);
    if (old) {
      old.at = Math.max(old.at, at);
      old.kind = it.kind;
      Object.assign(old, ad, { track: it.track ?? old.track ?? null, paired: !!it.paired, codecs: it.codecs ?? old.codecs, related: it.related ?? old.related, altOf: it.altOf ?? null });
    } else items.push({ url: it.url, kind: it.kind, size: it.size ?? null, at, ...ad, track: it.track ?? null, paired: !!it.paired, codecs: it.codecs, related: it.related, altOf: it.altOf ?? null });
  }
  // keep the background's ranking (main video first, suspected ads last)
  const pos = new Map(incoming.map((i, n) => [i.url, n]));
  items.sort((a, b) => (pos.get(a.url) ?? 1e9) - (pos.get(b.url) ?? 1e9));
  scheduleEvaluate();
  render();
}

chrome.runtime.onMessage.addListener((msg: { type: string; payload?: { items?: Item[]; selectors?: string[]; ytdlp?: boolean } }, _sender, sendResponse) => {
  if (msg.type === "ShowButton") {
    ytdlpOk = !!msg.payload?.ytdlp;
    merge(msg.payload?.items ?? [], msg.payload?.selectors);
    sendResponse({ ok: true });
  } else if (msg.type === "Placement") {
    setWinner((msg as { winner?: string | null }).winner ?? null);
  } else if (msg.type === "ProbeTrack") {
    // read the first bytes of a whole track file like the page's player does (page Referer / CORS)
    const url = String((msg as { url?: string }).url ?? "");
    if (!/^https?:/.test(url)) {
      sendResponse({ track: null });
      return;
    }
    fetch(url, { headers: { Range: "bytes=0-65535" }, credentials: "include" })
      .then((r) => (r.ok ? r.arrayBuffer() : null))
      .then((b) => {
        const i = b ? mp4Info(new Uint8Array(b.slice(0, 65536))) : null;
        sendResponse({ track: i?.track ?? null, duration: i?.duration ?? null, height: i?.height ?? null });
      })
      .catch(() => sendResponse({ track: null }));
    return true;
  } else if (msg.type === "CollectLinks") {
    sendResponse(collectLinks());
  } else if (msg.type === "GetPageTitles" && window === window.top) {
    sendResponse(pageTitles());
  }
  return false;
});

addEventListener("scroll", () => (place(), scheduleEvaluate()), { passive: true });
addEventListener("resize", () => (place(), scheduleEvaluate()), { passive: true });
// player swaps / an ad ending / a new stream: re-score (debounced)
for (const ev of ["play", "playing", "pause", "ended", "emptied", "loadedmetadata", "durationchange", "resize"]) document.addEventListener(ev, scheduleEvaluate, true);
document.addEventListener("fullscreenchange", () => (document.fullscreenElement ? hide() : render()));
/** Candidates arrived while no player was visible yet (e.g. a 0×0 <video> swapped in after a pre-roll): retry. */
function retryRender(): void {
  if (!dismissed && freshItems().length && (!host || !host.isConnected || host.style.display === "none")) scheduleEvaluate();
}
if (typeof ResizeObserver !== "undefined") {
  const ro = new ResizeObserver(() => (target && host?.isConnected ? place() : retryRender()));
  new MutationObserver(() => {
    for (const el of document.querySelectorAll("video")) ro.observe(el);
    if (localBest && !localBest.isConnected) scheduleEvaluate();
  }).observe(document.documentElement, { childList: true, subtree: true });
}
setInterval(() => {
  if (host && host.style.display !== "none" && !freshItems().length) hide();
  else if (freshItems().length) scheduleEvaluate();
}, 2_000);
setTimeout(reportGeometry, 1500);
// media seen before this script was injected (document_idle comes late on video pages)
if (window === window.top) {
  chrome.runtime
    .sendMessage({ type: "GetTabMedia" })
    .then((st: { items?: Item[]; selectors?: string[]; tabPaused?: boolean; siteDisabled?: boolean }) => {
      if (st && !st.tabPaused && !st.siteDisabled) merge(st.items ?? [], st.selectors);
    })
    .catch(() => undefined);
}

// optional page-world hook (pagehook.ts / shared/msehook.ts) → background
/** MSE observations by blob: URL and by MediaSource id; srcObject attachments by element index */
const mseByBlob = new Map<string, MseReport>();
const mseById = new Map<number, MseReport>();
const mseAttached = new Map<number, number | "worker">();
const str = (x: unknown, n: number) => (typeof x === "string" ? x.slice(0, n) : null);
function onHookReport(m: unknown) {
  const d = m as { __ldm?: string; url?: string } | null;
  if (d?.__ldm === PAGE_MSG && typeof d.url === "string") chrome.runtime.sendMessage({ type: "PageMedia", url: d.url }).catch(() => undefined);
  else if (d?.__ldm === MSE_MSG) {
    // the page can post anything here: keep only well-formed, bounded fields
    const r = d as unknown as Partial<MseReport>;
    if (typeof r.id !== "number") return;
    const rep: MseReport = {
      __ldm: MSE_MSG,
      id: r.id,
      blobs: (Array.isArray(r.blobs) ? r.blobs : []).map((b) => str(b, 300)).filter((b): b is string => !!b?.startsWith("blob:")).slice(0, 4),
      types: (Array.isArray(r.types) ? r.types : []).map((x) => str(x, 200)).filter((x): x is string => !!x).slice(0, 8),
      appends: Number(r.appends) || 0,
      bytes: Number(r.bytes) || 0,
      manifest: str(r.manifest, 2000),
      segments: (Array.isArray(r.segments) ? r.segments : []).map((x) => str(x, 2000)).filter((x): x is string => !!x).slice(0, MAX_SEGMENT_DIRS),
      latest: (Array.isArray(r.latest) ? r.latest : []).map((x) => str(x, 2000)).filter((x): x is string => !!x).slice(0, MAX_LATEST),
    };
    if (mseById.size > 50 && !mseById.has(rep.id)) return;
    mseById.set(rep.id, rep);
    for (const b of rep.blobs) mseByBlob.set(b, rep);
    signalsDirty = true;
  } else if (d?.__ldm === MSE_ATTACH_MSG) {
    const a = d as { id?: unknown; worker?: unknown; index?: unknown };
    if (typeof a.index === "number" && a.index >= 0) mseAttached.set(a.index, a.worker === true ? "worker" : typeof a.id === "number" ? a.id : -1);
  }
}

// reports arrive over the private channel (shared/bridge.ts) via the document_start bridge
// script, which buffered anything sent before this content script was injected
{
  const sink = hookSink(globalThis as unknown as Record<string, unknown>);
  sink.fn = onHookReport;
  for (const m of sink.q.splice(0)) onHookReport(m);
}

/** The MSE observation of a media element: by its blob: currentSrc, or a srcObject attachment. */
function mseOf(el: HTMLMediaElement): MseLink | null {
  let r = el.currentSrc?.startsWith("blob:") ? mseByBlob.get(el.currentSrc) : undefined;
  if (!r && !el.currentSrc && el.srcObject) {
    const a = mseAttached.get([...document.querySelectorAll("video, audio")].indexOf(el));
    if (a === "worker") return { worker: true, codecs: [] };
    if (typeof a === "number") r = mseById.get(a);
  }
  if (!r) return null;
  return { codecs: codecsOf(r.types), manifest: r.manifest, segments: r.segments, latest: r.latest, bytes: r.bytes, appends: r.appends };
}

// ---- ad-ranking page signals (shared/adscore.ts): players, "Skip Ad" controls, declared duration ----
const records = new Map<string, PlayerRecord>();
const skipWindows: [number, number][] = [];
let skipOpen: number | null = null;
let signalsDirty = false;
let lastSignalsSent = 0;

function declaredDuration(): number | null {
  if (window !== window.top) return null;
  const meta = (sel: string) => document.querySelector<HTMLMetaElement>(sel)?.content ?? null;
  const d = parseDuration(meta('meta[property="og:video:duration"], meta[property="video:duration"]')) ?? parseDuration(meta('meta[itemprop="duration"]'));
  if (d) return d;
  for (const s of document.querySelectorAll('script[type="application/ld+json"]')) {
    try {
      const walk = (x: unknown): number | null => {
        if (!x || typeof x !== "object") return null;
        if (Array.isArray(x)) {
          for (const y of x) {
            const r = walk(y);
            if (r) return r;
          }
          return null;
        }
        const o = x as Record<string, unknown>;
        const type = String(o["@type"] ?? "");
        if (/VideoObject/i.test(type) && typeof o.duration === "string") return parseDuration(o.duration);
        return walk(o["@graph"]) ?? walk(o.video);
      };
      const r = walk(JSON.parse(s.textContent || "null"));
      if (r) return r;
    } catch {
      /* invalid JSON-LD */
    }
  }
  return null;
}

/** Is a "Skip Ad" style control visible in (or around) a player? Only short texts near media are checked. */
function skipControlVisible(): boolean {
  const roots = new Set<Element>();
  for (const v of document.querySelectorAll("video")) {
    let el: Element | null = v;
    for (let i = 0; i < 4 && el?.parentElement; i++) el = el.parentElement;
    if (el) roots.add(el);
  }
  for (const root of roots) {
    let n = 0;
    for (const e of root.querySelectorAll<HTMLElement>("button, [role=button], a, div, span")) {
      if (++n > 600) break;
      const txt = e.textContent ?? "";
      const aria = e.getAttribute("aria-label") ?? "";
      if (txt.length > 40 && !aria) continue;
      if (!SKIP_CONTROL_RE.test(txt) && !SKIP_CONTROL_RE.test(aria)) continue;
      const r = e.getBoundingClientRect();
      if (r.width > 0 && r.height > 0 && getComputedStyle(e).visibility !== "hidden") return true;
    }
  }
  return false;
}

function sampleSignals(): void {
  const media = document.querySelectorAll<HTMLMediaElement>("video, audio");
  if (!media.length && !records.size) return;
  const now = Date.now();
  const skip = media.length ? skipControlVisible() : false;
  if (skip && skipOpen == null) skipOpen = now;
  if (!skip && skipOpen != null) {
    skipWindows.push([skipOpen, now]);
    skipOpen = null;
    signalsDirty = true;
  }
  const playingNow = new Set<PlayerRecord>();
  for (const el of media) {
    // a MediaSource attached through srcObject has no currentSrc: give it a blob-like id
    const srcObj = !el.currentSrc && el.srcObject && !(typeof MediaStream !== "undefined" && el.srcObject instanceof MediaStream);
    const src = el.currentSrc || (srcObj ? `blob:ldm-srcobject-${ensureId(el)}` : "");
    if (el.paused || el.readyState < 3 || !src) continue;
    const key = `${ensureId(el)}|${src}`;
    let rec = records.get(key);
    if (!rec) {
      rec = { src, el: ensureId(el), area: 0, played: 0, firstPlayAt: now };
      records.set(key, rec);
      if (records.size > 30) records.delete(records.keys().next().value as string);
    }
    const r = el.getBoundingClientRect();
    const vis = Math.max(0, Math.min(r.right, innerWidth) - Math.max(r.left, 0)) * Math.max(0, Math.min(r.bottom, innerHeight) - Math.max(r.top, 0));
    rec.area = Math.max(rec.area, Math.round(vis));
    rec.played += 1;
    rec.lastPlayAt = now;
    if (Number.isFinite(el.duration) && el.duration > 0) rec.duration = Math.round(el.duration * 100) / 100;
    if (skip) rec.duringSkip = true;
    // structural ad context only (an "Ad" label is transient: a pre-roll in the main player)
    if (!rec.adContext) rec.adContext = frameAd ?? containerAdToken(el as unknown as ElLike);
    rec.playing = true;
    playingNow.add(rec);
    const mse = mseOf(el);
    if (mse) rec.mse = mse;
    signalsDirty = true;
  }
  for (const rec of records.values())
    if (rec.playing && !playingNow.has(rec)) {
      rec.playing = false;
      signalsDirty = true;
    }
  if (signalsDirty && now - lastSignalsSent >= 2000) {
    signalsDirty = false;
    lastSignalsSent = now;
    const wins = skipOpen != null ? [...skipWindows, [skipOpen, now] as [number, number]] : skipWindows;
    chrome.runtime
      .sendMessage({ type: "PageSignals", signals: { declaredDuration: declaredDuration(), players: [...records.values()], skipWindows: wins } })
      .catch(() => undefined);
  }
}
setInterval(sampleSignals, 1000);
