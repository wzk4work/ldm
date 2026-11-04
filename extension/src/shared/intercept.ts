// downloads interception + webRequest media sniffing.
import type { DownloadAdd, RuleSet } from "./protocol";
import { CompiledRules } from "./rules";
import { classify, dispositionFilename, mediaKind, urlExt } from "./classify";
import type { MediaRegistry } from "./media";
import { modifierAction, type Mods } from "./policy";

export interface InterceptDeps {
  rules(): CompiledRules;
  hostReady(): boolean;
  addDownload(req: DownloadAdd): Promise<"take" | "pass">;
  cookiesFor(url: string): Promise<string | null>;
  /** modifier keys held when the user clicked the link that started this download */
  clickModifiers?(item: chrome.downloads.DownloadItem): Mods | undefined;
  /** per-tab / per-site switch (false = leave the download to the browser) */
  allowed?(item: chrome.downloads.DownloadItem): boolean;
}

/** Recently handed-over URLs, so our own resume/re-creation is not re-captured. */
const recentlyPassed = new Map<string, number>();

export function fromDownloadItem(item: chrome.downloads.DownloadItem, cookies: string | null): DownloadAdd {
  const name = item.filename ? item.filename.split(/[\\/]/).pop() : null;
  return {
    url: item.finalUrl || item.url,
    referrer: item.referrer || null,
    cookies,
    mime: item.mime || null,
    filename_hint: name || null,
    bytes_total: item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : null,
    request_id: String(item.id),
    user_agent: typeof navigator !== "undefined" ? navigator.userAgent : null,
    source: "extension",
  };
}

/** Ask LDM whether it takes this download. `null` means we never considered it. */
async function decide(deps: InterceptDeps, item: chrome.downloads.DownloadItem): Promise<"take" | "pass" | null> {
  const url = item.finalUrl || item.url;
  const passedAt = recentlyPassed.get(url);
  if (passedAt && Date.now() - passedAt < 30_000) return null;
  if (!deps.hostReady()) return null;
  const r = deps.rules();
  const mode = modifierAction(deps.clickModifiers?.(item), r.rules.bypassModifier, r.rules.forceModifier);
  if (mode === "bypass") return null;
  const size = item.totalBytes > 0 ? item.totalBytes : null;
  if (mode === "force") {
    if (!/^https?:/i.test(url)) return null;
  } else {
    if (deps.allowed && !deps.allowed(item)) return null;
    if (!r.shouldCapture(url, item.mime, size, item.filename)) return null;
  }
  try {
    return await deps.addDownload(fromDownloadItem(item, await deps.cookiesFor(url)));
  } catch {
    return "pass";
  }
}

async function dropBrowserCopy(id: number): Promise<void> {
  await chrome.downloads.cancel(id).catch(() => undefined);
  // if the transfer already completed, cancel is a no-op: remove the file instead
  await chrome.downloads.removeFile(id).catch(() => undefined);
  await chrome.downloads.erase({ id }).catch(() => undefined);
}

/** One decision per browser download id, shared by both hooks below. */
const decisions = new Map<number, Promise<"take" | "pass" | null>>();

function decisionFor(deps: InterceptDeps, item: chrome.downloads.DownloadItem): Promise<"take" | "pass" | null> {
  let p = decisions.get(item.id);
  if (p) return p;
  const dl = chrome.downloads;
  p = (async () => {
    let paused = true;
    await dl.pause(item.id).catch(() => {
      paused = false;
    });
    const action = await decide(deps, item);
    if (action === "take") {
      await dropBrowserCopy(item.id);
    } else {
      if (action === "pass") recentlyPassed.set(item.finalUrl || item.url, Date.now());
      if (paused) await dl.resume(item.id).catch(() => undefined);
    }
    return action;
  })();
  decisions.set(item.id, p);
  void p.finally(() => setTimeout(() => decisions.delete(item.id), 60_000));
  return p;
}

export function installDownloadInterception(deps: InterceptDeps): void {
  const dl = chrome.downloads;
  // Every browser: pause as soon as the download appears, then hand over or resume.
  dl.onCreated.addListener((item) => {
    void decisionFor(deps, item);
  });
  // Chromium also lets us hold the filename suggestion, which keeps even a fast
  // local transfer from completing before LDM has answered.
  dl.onDeterminingFilename?.addListener((item, suggest) => {
    void decisionFor(deps, item).then((action) => {
      if (action !== "take") suggest();
    });
    return true;
  });
}

export interface SniffDeps {
  registry: MediaRegistry;
  rules(): RuleSet;
  onMedia(tabId: number, changed: boolean): void;
  /** a whole single-track file was found (its track type still has to be read) */
  onTrackFile?(tabId: number, url: string): void;
  /** site adapters can mark extra URLs (by pattern) as media manifests */
  adapterMatch?(tabId: number, url: string): boolean;
}

/** Minimum total size of a Range-fetched ".m4s" for it to count as a whole track file. */
export const WHOLE_TRACK_MIN = 1024 * 1024;

/**
 * A stream "segment" URL (.m4s/.m4f) answered to a Range request whose Content-Range total is
 * large is not a segment but a whole single-track file (seen on Bilibili-style players that read
 * one fMP4 per track in chunks). Returns its total size, or null.
 */
export function wholeTrackTotal(url: string, verdict: string, contentRange: string | null): number | null {
  if (verdict !== "segment" || !contentRange) return null;
  const ext = urlExt(url);
  if (ext !== "m4s" && ext !== "m4f") return null;
  const m = /^bytes\s+\d+-\d+\/(\d+)\s*$/i.exec(contentRange.trim());
  const total = m ? Number(m[1]) : NaN;
  return Number.isFinite(total) && total >= WHOLE_TRACK_MIN ? total : null;
}

/** Observe response headers to find media and manifests (non-blocking). */
export function installMediaSniffer(deps: SniffDeps): void {
  const adapterHit = deps.adapterMatch ?? (() => false);
  chrome.webRequest.onHeadersReceived.addListener(
    (d) => {
      if (d.tabId < 0 || d.statusCode >= 400) return;
      const h = (n: string) => d.responseHeaders?.find((x) => x.name.toLowerCase() === n)?.value ?? null;
      const mime = h("content-type");
      let verdict = classify(d.url, mime, h("content-disposition"), deps.rules());
      if (verdict !== "media" && verdict !== "manifest" && adapterHit(d.tabId, d.url)) verdict = "manifest";
      const whole = wholeTrackTotal(d.url, verdict, h("content-range"));
      if (whole != null) {
        // a ".m4s" that is a whole file (Range-fetched, large total): one video-only or audio-only track
        const changed = deps.registry.add(d.tabId, { url: d.url, kind: "progressive", mime, size: whole, title: null });
        if (changed) deps.onTrackFile?.(d.tabId, d.url);
        deps.onMedia(d.tabId, changed);
        return;
      }
      if (verdict !== "media" && verdict !== "manifest") return;
      const len = Number(h("content-length") ?? "");
      const range = h("content-range");
      const total = range && /\/(\d+)$/.test(range) ? Number(/\/(\d+)$/.exec(range)![1]) : Number.isFinite(len) && len > 0 ? len : null;
      if (verdict === "media" && total != null && total < deps.rules().minSize) return;
      const changed = deps.registry.add(d.tabId, {
        url: d.url,
        kind: mediaKind(d.url, mime),
        mime,
        size: total,
        title: dispositionFilename(h("content-disposition")),
      });
      deps.onMedia(d.tabId, changed);
    },
    { urls: ["http://*/*", "https://*/*"], types: ["media", "xmlhttprequest", "other", "object", "sub_frame"] },
    ["responseHeaders"],
  );
}
