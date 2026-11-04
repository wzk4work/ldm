// LDM extension service worker / background script.
import { pickTitle, type PageTitles } from "./shared/title";
import { DEFAULT_RULES, envelope, type DownloadAdd, type Envelope, type InterceptDecision, type RuleSet } from "./shared/protocol";
import { HostClient, NativeTransport, WsTransport, type Transport } from "./shared/native";
import { CompiledRules } from "./shared/rules";
import { MediaRegistry, hlsChildUris, hlsDuration, hlsFirstVariant, playlistBodyKind } from "./shared/media";
import { isAdRequestUrl, type PageSignals } from "./shared/adscore";
import { chooseWinner, type FrameReport } from "./shared/placement";
import { installDownloadInterception, installMediaSniffer } from "./shared/intercept";
import { createMenus, MENU, urlsInText } from "./shared/menus";
import { storeExcluded, ytdlpAvailable, ytdlpPageUrl } from "./shared/ytdlp";
import { setHostStatus, setTabDisabled } from "./shared/badge";
import { classify, mediaKind } from "./shared/classify";
import { adapterMatchesUrl, adaptersFor, ClickLog, hostListed, hostOf, proxyInfoFrom, type Mods } from "./shared/policy";

let rules: RuleSet = { ...DEFAULT_RULES };
let compiled = new CompiledRules(rules);
const registry = new MediaRegistry();

interface Settings {
  intercept: boolean;
  wsFallback: boolean;
  wsPort: number;
  wsToken: string;
  /** "在此站点禁用" list, kept in the extension (the daemon list is in LDM 选项) */
  disabledHosts: string[];
  /** show the raw candidate list in the popup */
  debug: boolean;
  /** optional page-world fetch/XHR hook */
  pageHooks: boolean;
}
let settings: Settings = { intercept: true, wsFallback: true, wsPort: 17001, wsToken: "", disabledHosts: [], debug: false, pageHooks: false };

const HOOK_ID = "ldm-pagehook";
/** ISOLATED-world half of the private channel (shared/bridge.ts), registered with the hook */
const BRIDGE_ID = "ldm-pagebridge";
const HOOK_IDS = [BRIDGE_ID, HOOK_ID];
async function syncPageHooks(): Promise<void> {
  const sc = (chrome as unknown as { scripting?: typeof chrome.scripting }).scripting;
  if (!sc?.registerContentScripts) return;
  try {
    const have = (await sc.getRegisteredContentScripts({ ids: HOOK_IDS })).map((r) => r.id);
    const matches = ["http://*/*", "https://*/*"];
    if (settings.pageHooks && have.length < HOOK_IDS.length) {
      if (have.length) await sc.unregisterContentScripts({ ids: have });
      await sc.registerContentScripts([
        { id: BRIDGE_ID, js: ["pagebridge.js"], matches, runAt: "document_start", allFrames: true },
        { id: HOOK_ID, js: ["pagehook.js"], matches, runAt: "document_start", allFrames: true, world: "MAIN" } as chrome.scripting.RegisteredContentScript,
      ]);
    } else if (!settings.pageHooks && have.length) {
      await sc.unregisterContentScripts({ ids: have });
    }
  } catch (e) {
    console.warn("LDM page hook:", e);
  }
}
/** tabs where the user paused interception (per-tab switch); survives service-worker restarts via storage.session */
const pausedTabs = new Set<number>();
const tabUrls = new Map<number, string>();
const clicks = new ClickLog();
const session: chrome.storage.StorageArea | undefined = (chrome.storage as { session?: chrome.storage.StorageArea }).session;

function tabOff(tabId: number | undefined): boolean {
  if (tabId == null || tabId < 0) return false;
  return pausedTabs.has(tabId) || hostListed(hostOf(tabUrls.get(tabId)), settings.disabledHosts);
}

function refreshBadge(tabId: number): void {
  setTabDisabled(tabId, tabOff(tabId), registry.list(tabId).length);
}

function persistSession(): void {
  session?.set({ pausedTabs: [...pausedTabs] }).catch(() => undefined);
}

function browserName(): string {
  const ua = navigator.userAgent;
  if (/Firefox\//.test(ua)) return "firefox";
  if (/Edg\//.test(ua)) return "edge";
  return /Chromium\//.test(ua) ? "chromium" : "chrome";
}

const factories: Array<() => Transport> = [() => new NativeTransport()];
factories.push(() => {
  if (!settings.wsFallback) throw new Error("ws fallback disabled");
  return new WsTransport(settings.wsPort, settings.wsToken || undefined);
});

const host = new HostClient(factories, () =>
  envelope("Hello", { browser: browserName(), version: chrome.runtime.getManifest().version, extensionId: chrome.runtime.id, uiLanguage: chrome.i18n.getUILanguage() }),
);

host.onStatus((up, via) => {
  setHostStatus(up, via);
  if (up) {
    void resendPending();
    void sendProxyInfo();
  }
});

/** Browser proxy sync (optional "proxy" permission): lets the daemon follow the browser's proxy. */
async function sendProxyInfo(): Promise<void> {
  const px = (chrome as unknown as { proxy?: typeof chrome.proxy }).proxy;
  if (!px?.settings || !host.ready) return;
  try {
    const d = await new Promise<{ value?: unknown; levelOfControl?: string }>((resolve) => px.settings.get({}, (x) => resolve(x ?? {})));
    host.notify("ProxyInfo", { ...proxyInfoFrom(d.value), levelOfControl: d.levelOfControl });
  } catch {
    /* permission not granted */
  }
}
try {
  (chrome as unknown as { proxy?: typeof chrome.proxy }).proxy?.settings.onChange.addListener(() => void sendProxyInfo());
  chrome.permissions?.onAdded?.addListener(() => void sendProxyInfo());
} catch {
  /* proxy API unavailable */
}
host.onEvent((e: Envelope) => {
  switch (e.type) {
    case "RuleSetPush": {
      rules = { ...DEFAULT_RULES, ...(e.payload as Partial<RuleSet>) };
      registry.adSettings = { enabled: rules.adRanking !== false, userDomains: (rules.adDomains ?? []).slice(0, 20000) };
      compiled = new CompiledRules(rules);
      chrome.storage.local.set({ rules }).catch(() => undefined);
      for (const [t, u] of tabUrls) syncYtdlpEntry(t, u);
      break;
    }
    case "ShowMediaButton": {
      const tabId = Number((e.payload as { tabId?: number }).tabId);
      if (!(tabId > 0)) break;
      // the daemon's payload only holds the candidate it just recorded; the page button takes the
      // tab's full list as authoritative (it drops unlisted rows), so send our own ranked list when
      // we have one — a daemon echo must never shrink the menu to a single (possibly ad) row
      const own = registry.list(tabId);
      const yt = ytdlpAvailable(tabUrls.get(tabId), rules);
      const payload = own.length ? { ...(e.payload as object), items: own, ytdlp: yt } : { ...(e.payload as object), ytdlp: yt };
      chrome.tabs.sendMessage(tabId, { type: "ShowButton", payload }).catch(() => undefined);
      break;
    }
    case "Notify": {
      const p = e.payload as { title?: string; body?: string | null; jobId?: string; code?: string };
      // completions are announced by the desktop (LDM tray); the browser only shows failures
      if (!p.code) break;
      try {
        chrome.notifications?.create(`ldm-${p.jobId ?? Date.now()}`, { type: "basic", iconUrl: "icons/icon128.png", title: p.title || "LDM", message: String(p.body ?? "") });
      } catch {
        /* notifications unavailable */
      }
      break;
    }
    case "GetCookies": {
      // the daemon asks for fresh cookies (e.g. a session expired mid-download)
      const p = e.payload as { url?: string; jobId?: string; requestId?: string };
      if (p.url) void cookiesFor(p.url).then((cookies) => host.notify("CookiesQueryResult", { url: p.url, jobId: p.jobId, requestId: p.requestId, cookies }));
      break;
    }
  }
});

async function cookiesFor(url: string): Promise<string | null> {
  try {
    const list = await chrome.cookies.getAll({ url });
    return list.length ? list.map((c) => `${c.name}=${c.value}`).join("; ") : null;
  } catch {
    return null;
  }
}

/** Requests not yet answered are stashed in storage.session so a restarted service worker can resend them. */
async function stash(key: string, req: DownloadAdd | null): Promise<void> {
  if (!session) return;
  const cur = ((await session.get("pending")).pending ?? {}) as Record<string, DownloadAdd>;
  if (req) cur[key] = req;
  else delete cur[key];
  await session.set({ pending: cur });
}

async function addDownload(req: DownloadAdd): Promise<"take" | "pass"> {
  if (!host.ready) host.connect();
  const key = `${req.url}#${req.request_id ?? Date.now()}`;
  await stash(key, req).catch(() => undefined);
  try {
    const r = await host.request("DownloadAdd", req, 10000);
    if (r.type === "InterceptDecision") return (r.payload as unknown as InterceptDecision).action;
    return "pass";
  } finally {
    await stash(key, null).catch(() => undefined);
  }
}

async function resendPending(): Promise<void> {
  if (!session) return;
  const cur = ((await session.get("pending")).pending ?? {}) as Record<string, DownloadAdd>;
  await session.set({ pending: {} });
  for (const req of Object.values(cur)) await addDownload({ ...req, request_id: null }).catch(() => undefined);
}

/**
 * Send a URL from a tab to the daemon. `media` marks a media candidate (in-page button,
 * popup, "download this video" menu): it is named after the page's video title,
 * for direct mp4/webm files too. Plain links keep the server / URL name.
 */
async function downloadFromTab(
  tabId: number | undefined,
  url: string,
  extra: Partial<DownloadAdd> = {},
  media = false,
  titles?: PageTitles | null,
): Promise<void> {
  let page: chrome.tabs.Tab | undefined;
  if (tabId != null && tabId >= 0) page = await chrome.tabs.get(tabId).catch(() => undefined);
  const k = mediaKind(url);
  let title: string | null = null;
  if (media || k !== "progressive") {
    title = pickTitle(page?.title, page?.url, titles) || null;
    if (!title && !titles && tabId != null && tabId >= 0) {
      // popup / context menu: ask the page (top frame) for its visible title
      const t = (await chrome.tabs.sendMessage(tabId, { type: "GetPageTitles" }, { frameId: 0 }).catch(() => null)) as PageTitles | null;
      title = pickTitle(page?.title, page?.url, t) || null;
    }
  }
  // a video-only track file: the page's audio track file goes with it (merged by LDM)
  const own = media && tabId != null && tabId >= 0 ? registry.list(tabId).find((i) => i.url === url.split("#")[0]) : undefined;
  if (own?.kind === "ytdlp" || extra.kind === "ytdlp") {
    // yt-dlp gets the page URL only; browser cookies are never forwarded (ytdlp.cookies_from_browser is the opt-in)
    await addDownload({
      url,
      referrer: page?.url ?? null,
      page_url: page?.url ?? null,
      tab_id: tabId ?? null,
      title_hint: title ?? pickTitle(page?.title, page?.url, titles) ?? null,
      source: "extension",
      ...extra,
      kind: "ytdlp",
    });
    return;
  }
  const audioUrl = own?.track === "video" && tabId != null ? registry.audioTrackFor(tabId) : null;
  if (audioUrl) extra = { audio_url: audioUrl, filename_hint: title, ...extra };
  // a whole track file (".m4s") listed as a candidate is a plain file download, not a stream segment
  else if (own && own.kind === "progressive" && /\.m4[sf]$/i.test(new URL(own.url).pathname)) extra = { kind: "http", ...extra };
  await addDownload({
    url,
    referrer: page?.url ?? null,
    page_url: page?.url ?? null,
    cookies: await cookiesFor(url),
    user_agent: navigator.userAgent,
    tab_id: tabId ?? null,
    kind: k === "progressive" ? null : k,
    filename_hint: k === "progressive" ? null : title,
    title_hint: title,
    ad_reasons: media && tabId != null ? (registry.list(tabId).find((i) => i.url === url.split("#")[0] && i.ad)?.adReasons ?? null) : null,
    source: "extension",
    ...extra,
  });
}

function clickFor(item: chrome.downloads.DownloadItem) {
  return clicks.find([item.url, item.finalUrl]);
}

installDownloadInterception({
  rules: () => compiled,
  hostReady: () => host.ready && settings.intercept,
  addDownload,
  cookiesFor,
  clickModifiers: (item): Mods | undefined => clickFor(item)?.mods,
  allowed: (item) => {
    const c = clickFor(item);
    if (c && tabOff(c.tabId)) return false;
    const pageHost = hostOf(item.referrer);
    if (hostListed(pageHost, settings.disabledHosts) || hostListed(hostOf(item.finalUrl || item.url), settings.disabledHosts)) return false;
    for (const t of pausedTabs) if (item.referrer && tabUrls.get(t) === item.referrer) return false;
    return true;
  },
});

function adaptersForTab(tabId: number) {
  return adaptersFor(hostOf(tabUrls.get(tabId)), rules.siteAdapters);
}

const announced = new Map<number, number>();
const lastPush = new Map<number, number>();
const probed = new Set<string>();
function onMedia(tabId: number, changed: boolean): void {
    if (storeExcluded(tabUrls.get(tabId))) return; // store build: nothing is offered on these pages
    const items = registry.list(tabId);
    // the same video at several bitrates: read each file's MP4 header (duration, resolution) once
    const grouped = new Set(items.flatMap((i) => (i.altOf ? [i.url, i.altOf] : [])));
    // several playable files: their headers tell whether they are one video at several bitrates
    const files = items.filter((i) => i.kind === "progressive" && !i.ad);
    if (files.length > 1) for (const f of files.slice(0, 6)) grouped.add(f.url);
    for (const u of grouped)
      if (!probed.has(`${tabId} ${u}`)) {
        probed.add(`${tabId} ${u}`);
        if (probed.size > 2000) probed.clear();
        void probeTrack(tabId, u);
      }
    refreshBadge(tabId);
    // unchanged candidates are re-announced at most every 30 s so the page button stays "fresh"
    const pushed = lastPush.get(tabId) ?? 0;
    if (tabOff(tabId) || (!changed && Date.now() - pushed < 30_000)) return;
    lastPush.set(tabId, Date.now());
    const selectors = adaptersForTab(tabId).flatMap((a) => a.detect?.elementSelectors ?? []);
    chrome.tabs.sendMessage(tabId, { type: "ShowButton", payload: { tabId, items, selectors, ytdlp: ytdlpAvailable(tabUrls.get(tabId), rules) } }).catch(() => undefined);
    announceToDaemon(tabId);
}
const announceTimers = new Map<number, ReturnType<typeof setTimeout>>();
/** Tell the daemon about a tab's candidates: at most once a second, with a trailing update so the
 *  last re-ranking (scores / ad verdicts) is never dropped by the rate limit. */
function announceToDaemon(tabId: number): void {
  if (!host.ready) return;
  const wait = 1000 - (Date.now() - (announced.get(tabId) ?? 0));
  if (wait > 0) {
    if (!announceTimers.has(tabId)) announceTimers.set(tabId, setTimeout(() => (announceTimers.delete(tabId), announceToDaemon(tabId)), wait + 10));
    return;
  }
  announced.set(tabId, Date.now());
  // the yt-dlp page entry is extension-side only (the daemon routes the page URL itself)
  const items = registry.list(tabId).filter((i) => i.kind !== "ytdlp");
  chrome.tabs.get(tabId).then((t) => {
    for (const it of items.slice(0, 10)) host.notify("MediaCandidate", registry.toCandidate(tabId, { ...it, title: it.title ?? t.title ?? null }, t.url));
  }).catch(() => undefined);
}
/** Fetch a newly seen HLS playlist once; if it is a master, its variant playlists are folded into it. */
async function fetchText(url: string): Promise<{ text: string; url: string } | null> {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 8000);
  try {
    const r = await fetch(url, { credentials: "include", signal: ctl.signal });
    if (!r.ok || Number(r.headers.get("content-length") ?? 0) > 2_000_000) return null;
    return { text: await r.text(), url: r.url || url };
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Fetch a newly seen HLS playlist once: a master folds its variant playlists into itself, and
 * the duration (from the first variant / the playlist itself) feeds the ad ranking.
 */
async function inspectHls(tabId: number, url: string): Promise<void> {
  try {
    const r = await fetchText(url);
    if (!r) return;
    const body = playlistBodyKind(r.text);
    if (body) {
      // not media at all (seen: a VMAP ad-break description served under an .m3u8 path):
      // keep the row (nothing is hidden) but rank it as what it is
      if (body === "vast") registry.noteAdRequest(tabId);
      if (registry.setNotPlaylist(tabId, url, body)) onMedia(tabId, true);
      return;
    }
    const kids = hlsChildUris(r.text, r.url);
    let changed = !!kids && registry.setChildren(tabId, url, kids);
    let dur = hlsDuration(r.text);
    const v = kids ? hlsFirstVariant(r.text, r.url) : null;
    if (v) {
      const vr = await fetchText(v).catch(() => null);
      dur = vr ? hlsDuration(vr.text) : null;
    }
    if (registry.setDuration(tabId, url, dur)) changed = true;
    if (changed) onMedia(tabId, true);
  } catch {
    /* network errors: keep listing as-is */
  }
}

// VAST / VMAP / IMA requests mark an ad break on the tab (ad ranking signal 5)
chrome.webRequest.onBeforeRequest.addListener(
  (d) => {
    if (d.tabId >= 0 && isAdRequestUrl(d.url)) registry.noteAdRequest(d.tabId, d.timeStamp || Date.now());
  },
  { urls: ["http://*/*", "https://*/*"] },
);

installMediaSniffer({
  registry,
  rules: () => rules,
  adapterMatch: (tabId, url) => adapterMatchesUrl(url, adaptersForTab(tabId)),
  onMedia: (tabId, changed) => {
    for (const it of registry.list(tabId)) if (it.kind === "hls" && registry.needsInspection(tabId, it.url)) void inspectHls(tabId, it.url);
    onMedia(tabId, changed);
  },
  onTrackFile: (tabId, url) => void probeTrack(tabId, url),
});

/** Which track a whole single-track file holds: the page's content script reads the file's first
 *  bytes (same request the page's own player makes, with the page's Referer/CORS) and its MP4 header. */
async function probeTrack(tabId: number, url: string): Promise<void> {
  const r = (await chrome.tabs.sendMessage(tabId, { type: "ProbeTrack", url }, { frameId: 0 }).catch(() => null)) as {
    track?: string | null;
    duration?: number | null;
    height?: number | null;
  } | null;
  const t = r?.track;
  const num = (x: unknown) => (typeof x === "number" && Number.isFinite(x) && x > 0 ? x : null);
  const info = registry.setMp4Info(tabId, url, { duration: num(r?.duration), height: num(r?.height) });
  if (((t === "video" || t === "audio" || t === "muxed") && registry.setTrack(tabId, url, t)) || info) onMedia(tabId, true);
}

// ---- one in-page button per tab: every frame reports its best player, the background picks one ----
const placements = new Map<number, Map<number, FrameReport>>();
const placementWinner = new Map<number, string | null>();
function clearPlacements(tabId: number): void {
  placements.delete(tabId);
  placementWinner.delete(tabId);
}
function updatePlacement(tabId: number, frameId: number, report: FrameReport | null, frameUrl?: string): string | null {
  let m = placements.get(tabId);
  if (!m) placements.set(tabId, (m = new Map()));
  if (report) m.set(frameId, { ...report, frameUrl: frameUrl ?? report.frameUrl, at: Date.now() });
  else m.delete(frameId);
  const prev = placementWinner.get(tabId) ?? null;
  const w = chooseWinner([...m.values()], prev);
  if (w !== prev) {
    placementWinner.set(tabId, w);
    chrome.tabs.sendMessage(tabId, { type: "Placement", winner: w }).catch(() => undefined);
  }
  return w;
}
/** frame URLs a parent frame flagged as ad iframes (ad container around the <iframe>) */
function parentFlaggedAd(tabId: number, url: string | undefined): boolean {
  if (!url) return false;
  const u = url.split("#")[0];
  for (const r of placements.get(tabId)?.values() ?? []) if ((r.adIframes ?? []).some((x) => x.split("#")[0] === u)) return true;
  return false;
}

function tabState(tabId: number, extra: Record<string, unknown>): void {
  if (host.ready) host.notify("TabState", { tab_id: tabId, ...extra });
}

chrome.tabs.onRemoved.addListener((tabId) => {
  registry.clear(tabId);
  clearPlacements(tabId);
  tabUrls.delete(tabId);
  if (pausedTabs.delete(tabId)) persistSession();
  tabState(tabId, { closed: true });
});
/** On a yt-dlp video page (YouTube watch/shorts, `always` sites) list the page itself as a candidate. */
function syncYtdlpEntry(tabId: number, url: string | undefined, title?: string): void {
  const page = ytdlpPageUrl(url, rules);
  const stale = registry.list(tabId).filter((i) => i.kind === "ytdlp" && i.url !== page);
  for (const s of stale) registry.remove(tabId, s.url);
  if (page) onMedia(tabId, registry.add(tabId, { url: page, kind: "ytdlp", title: title ?? null }) || stale.length > 0);
  else if (stale.length) onMedia(tabId, true);
}
chrome.tabs.onUpdated.addListener((tabId, info, tab) => {
  if (tab.url) tabUrls.set(tabId, tab.url);
  if (info.status === "loading" && info.url) {
    registry.clear(tabId);
    clearPlacements(tabId);
    refreshBadge(tabId);
  }
  // YouTube navigates in-page (history API): a URL change without a reload
  if (info.url || info.status === "complete" || info.title) syncYtdlpEntry(tabId, tab.url, tab.title);
  if (info.status === "complete") {
    refreshBadge(tabId);
    tabState(tabId, { url: tab.url ?? null, title: tab.title ?? null, active: tab.active, intercept: !tabOff(tabId) });
  }
});
chrome.tabs.onActivated.addListener(({ tabId }) => tabState(tabId, { active: true, url: tabUrls.get(tabId) ?? null, intercept: !tabOff(tabId) }));

// heartbeat (≈15 s): a dead host is noticed and reconnected even while idle
setInterval(() => {
  if (!host.ready) return;
  host.request("Ping", {}, 5000).catch(() => {
    host.disconnect();
    host.connect();
  });
}, 15_000);

chrome.runtime.onInstalled.addListener(() => createMenus());
chrome.runtime.onStartup?.addListener(() => createMenus());

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const tabId = tab?.id;
  try {
    if (info.menuItemId === MENU.link && info.linkUrl) await downloadFromTab(tabId, info.linkUrl);
    else if (info.menuItemId === MENU.ytdlp) await downloadFromTab(tabId, info.linkUrl || info.frameUrl || info.pageUrl || tab?.url || "", { kind: "ytdlp" }, true);
    else if (info.menuItemId === MENU.media && info.srcUrl && /^https?:/i.test(info.srcUrl)) await downloadFromTab(tabId, info.srcUrl, {}, true);
    else if (info.menuItemId === MENU.media && tabId != null) {
      // blob:/MediaSource player: LDM can only use what it saw on the network
      const best = registry.list(tabId).find((i) => !i.ad);
      if (best) await downloadFromTab(tabId, best.url, {}, true);
      else if (rules.ytdlpAutoFallback && ytdlpAvailable(info.pageUrl, rules)) await downloadFromTab(tabId, info.pageUrl || tab?.url || "", { kind: "ytdlp" }, true);
    }
    else if (info.menuItemId === MENU.selection && info.selectionText) {
      for (const u of urlsInText(info.selectionText)) await downloadFromTab(tabId, u);
    } else if (info.menuItemId === MENU.allLinks && tabId != null) {
      // open the picker page: the user ticks what to download (type filter, select all)
      const links: { url: string; text?: string }[] = await chrome.tabs.sendMessage(tabId, { type: "CollectLinks" }).catch(() => []);
      const page = tab?.url ?? null;
      const picker = { page, title: tab?.title ?? "", tabId, links, preselect: links.filter((l) => compiled.shouldCapture(l.url)).map((l) => l.url) };
      await (session ?? chrome.storage.local).set({ picker });
      await chrome.tabs.create({ url: chrome.runtime.getURL("links.html") });
    }
  } catch (e) {
    console.warn("LDM:", e);
  }
});

// messages from content scripts and the popup
chrome.runtime.onMessage.addListener((msg: { type: string; [k: string]: unknown }, sender, sendResponse) => {
  const tabId = (msg.tabId as number | undefined) ?? sender.tab?.id;
  (async () => {
    switch (msg.type) {
      case "UserDownloadYtdlp": {
        const page = tabId != null ? tabUrls.get(tabId) ?? (await chrome.tabs.get(tabId).catch(() => undefined))?.url : undefined;
        const url = String(msg.url ?? "") || page || "";
        if (!/^https?:/i.test(url)) return { ok: false, error: "no page URL" };
        await downloadFromTab(tabId, url, { kind: "ytdlp", preferred_height: (msg.height as number | undefined) ?? null }, true, (msg.titles as PageTitles | undefined) ?? null);
        return { ok: true };
      }
      case "UserDownloadMedia":
        await downloadFromTab(tabId, String(msg.url), { preferred_height: (msg.height as number | undefined) ?? null }, true, (msg.titles as PageTitles | undefined) ?? null);
        return { ok: true };
      case "GetTabMedia": {
        const url = tabId != null ? tabUrls.get(tabId) ?? (await chrome.tabs.get(tabId).catch(() => undefined))?.url : undefined;
        if (tabId != null && url) tabUrls.set(tabId, url);
        const h = hostOf(url);
        return {
          items: tabId != null ? registry.list(tabId) : [],
          hostReady: host.ready,
          lastError: host.lastError,
          settings,
          tabPaused: tabId != null && pausedTabs.has(tabId),
          site: h,
          siteDisabled: hostListed(h, settings.disabledHosts),
          ytdlp: ytdlpAvailable(url, rules),
          selectors: tabId != null ? adaptersForTab(tabId).flatMap((a) => a.detect?.elementSelectors ?? []) : [],
        };
      }
      case "LinkClick":
        clicks.add({ url: String(msg.url), mods: (msg.mods as Mods) ?? {}, tabId, at: Date.now() });
        return { ok: true };
      case "ProbeMedia": {
        if (!host.ready) return { ok: false, error: "host not connected" };
        const page = tabId != null ? tabUrls.get(tabId) ?? null : null;
        const viaYtdlp = msg.kind === "ytdlp" || (tabId != null && registry.list(tabId).some((i) => i.url === msg.url && i.kind === "ytdlp"));
        const r = viaYtdlp
          ? await host.request("MediaProbe", { url: msg.url, kind: "ytdlp" }, 90000)
          : await host.request("MediaProbe", { url: msg.url, referrer: page, cookies: await cookiesFor(String(msg.url)), user_agent: navigator.userAgent }, 15000);
        return r.type === "MediaVariants" ? { ok: true, ...r.payload } : { ok: false, error: (r.payload as { error?: string }).error };
      }
      case "SetTabPaused":
        if (tabId != null) {
          if (msg.paused) pausedTabs.add(tabId);
          else pausedTabs.delete(tabId);
          persistSession();
          refreshBadge(tabId);
        }
        return { ok: true };
      case "SetSiteDisabled": {
        const h = String(msg.site || "");
        const list = settings.disabledHosts.filter((x) => x !== h);
        if (msg.disabled && h) list.push(h);
        settings = { ...settings, disabledHosts: list };
        await chrome.storage.local.set({ settings });
        if (tabId != null) refreshBadge(tabId);
        return { ok: true };
      }
      case "GetLinkPicker":
        return ((await (session ?? chrome.storage.local).get("picker")).picker ?? null) as object;
      case "DownloadLinks": {
        const p = msg as unknown as { urls: string[]; page: string | null };
        const items = await Promise.all(
          p.urls.map(async (url) => ({ url, referrer: p.page, page_url: p.page, cookies: await cookiesFor(url), user_agent: navigator.userAgent, source: "extension" })),
        );
        if (!host.ready) host.connect();
        if (items.length) await host.request("DownloadBatch", { items }, 15000);
        return { ok: true, count: items.length };
      }
      case "PageSignals": {
        // players / "Skip Ad" controls / declared duration from the content script
        if (tabId == null || tabId < 0) return { ok: false };
        const before = registry.rankSignature(tabId);
        const sig = msg.signals as PageSignals;
        // a frame its parent flagged as an ad iframe: its players play in ad context
        if (parentFlaggedAd(tabId, sender.url)) for (const p of sig.players ?? []) p.adContext ??= "ad-frame:parent-ad-container";
        registry.setSignals(tabId, sig, sender.frameId ?? 0);
        if (registry.rankSignature(tabId) !== before) onMedia(tabId, true);
        return { ok: true };
      }
      case "PlacementReport": {
        if (tabId == null || tabId < 0) return { winner: null };
        return { winner: updatePlacement(tabId, sender.frameId ?? 0, (msg.report as FrameReport | null) ?? null, sender.url) };
      }
      case "PageMedia": {
        // reported by the optional page-world hook via the content script
        const url = String(msg.url ?? "");
        if (!settings.pageHooks || tabId == null || tabId < 0 || !/^https?:/.test(url)) return { ok: false };
        if (classify(url, null, null, rules) !== "manifest") return { ok: false };
        onMedia(tabId, registry.add(tabId, { url, kind: mediaKind(url, null), mime: null, size: null, title: null }));
        return { ok: true };
      }
      case "MediaGeometry":
        if (host.ready) host.notify("MediaGeometry", { tab_id: tabId, ...(msg.payload as object) });
        return { ok: true };
      case "SetSettings":
        settings = { ...settings, ...(msg.settings as Partial<Settings>) };
        await chrome.storage.local.set({ settings });
        await syncPageHooks();
        return { ok: true, settings };
      case "Reconnect":
        host.disconnect();
        host.connect();
        return { ok: true };
      default:
        return { ok: false };
    }
  })()
    .then(sendResponse)
    .catch((e) => sendResponse({ ok: false, error: String(e) }));
  return true;
});

chrome.storage.local.get(["rules", "settings"]).then(async (s) => {
  if (s.rules) {
    rules = { ...DEFAULT_RULES, ...(s.rules as Partial<RuleSet>) };
    compiled = new CompiledRules(rules);
  }
  if (s.settings) settings = { ...settings, ...(s.settings as Partial<Settings>) };
  const ss = session ? await session.get("pausedTabs").catch(() => ({}) as Record<string, unknown>) : {};
  for (const t of ((ss as { pausedTabs?: number[] }).pausedTabs ?? [])) pausedTabs.add(t);
  await syncPageHooks();
  host.connect();
});
