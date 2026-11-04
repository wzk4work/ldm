// Per-click modifier keys, per-tab / per-site switches and site-adapter lookup.
// Pure helpers so they can be unit-tested outside the browser.
import { globToRegExp } from "./rules";

export interface Mods {
  alt?: boolean;
  shift?: boolean;
  ctrl?: boolean;
  insert?: boolean;
}

export interface Click {
  url: string;
  mods: Mods;
  tabId?: number;
  at: number;
}

/** What a held key means for the next download: let the browser keep it, force LDM, or no opinion. */
export function modifierAction(mods: Mods | undefined, bypass: string | undefined, force: string | undefined): "bypass" | "force" | null {
  if (!mods) return null;
  const held = (k: string | undefined) => !!k && k !== "none" && !!(mods as Record<string, boolean | undefined>)[k];
  if (held(bypass)) return "bypass";
  if (held(force)) return "force";
  return null;
}

/** Remembers link clicks for a few seconds so the download they start can be matched. */
export class ClickLog {
  private clicks: Click[] = [];
  constructor(private ttlMs = 8000) {}
  add(c: Click): void {
    this.prune(c.at);
    this.clicks.push(c);
  }
  /** Match by exact URL first, then by any URL of the redirect chain we know (url, finalUrl). */
  find(urls: (string | undefined | null)[], now = Date.now()): Click | undefined {
    this.prune(now);
    for (let i = this.clicks.length - 1; i >= 0; i--) {
      if (urls.some((u) => u && u === this.clicks[i].url)) return this.clicks[i];
    }
    return undefined;
  }
  private prune(now: number): void {
    this.clicks = this.clicks.filter((c) => now - c.at < this.ttlMs);
  }
}

export function hostOf(url: string | null | undefined): string {
  try {
    return url ? new URL(url).hostname.toLowerCase() : "";
  } catch {
    return "";
  }
}

/** Host list match: exact or "*.example.com" (which also covers example.com) or a plain parent domain. */
export function hostListed(host: string, list: string[]): boolean {
  if (!host) return false;
  return list.some((h) => {
    const p = h.trim().toLowerCase();
    if (!p) return false;
    if (p.startsWith("*.")) return host === p.slice(2) || host.endsWith(p.slice(1));
    return host === p || host.endsWith("." + p);
  });
}

export interface SiteAdapter {
  id?: string;
  matchHosts?: string[];
  detect?: { urlPatterns?: string[]; elementSelectors?: string[] };
  prefer?: { maxHeight?: number };
}

/** Adapters that apply to a page host. */
export function adaptersFor(host: string, adapters: unknown[] | undefined): SiteAdapter[] {
  const out: SiteAdapter[] = [];
  for (const a of adapters ?? []) {
    const s = a as SiteAdapter;
    if (!s || !Array.isArray(s.matchHosts)) continue;
    if (s.matchHosts.some((h) => globToRegExp(h).test(host) || (h.startsWith("*.") && host === h.slice(2)))) out.push(s);
  }
  return out;
}

/** Media URL patterns from adapters (e.g. a site's playlist path) – matching URLs are always treated as media. */
export function adapterMatchesUrl(url: string, adapters: SiteAdapter[]): boolean {
  return adapters.some((a) => (a.detect?.urlPatterns ?? []).some((p) => globToRegExp(p, true).test(url)));
}

export interface Quality {
  label: string;
  height?: number | null;
  bandwidth: number;
  kind: string;
}

/** "1080p · 5.2 Mbps", "仅音频 · 128 kbps". */
export function qualityLabel(q: Quality, audioOnly = "audio only"): string {
  const rate = q.bandwidth >= 1_000_000 ? `${(q.bandwidth / 1_000_000).toFixed(1)} Mbps` : q.bandwidth > 0 ? `${Math.round(q.bandwidth / 1000)} kbps` : "";
  // yt-dlp rows carry their own label ("1080p60 · avc1", "mp4a 129k")
  if (q.kind === "ytdlp") return q.label;
  if (q.kind === "ytdlp-audio") return `${audioOnly} · ${q.label}`;
  const head = q.kind === "audio" ? audioOnly : q.height ? `${q.height}p` : q.label || "?";
  return rate ? `${head} · ${rate}` : head;
}

/** Highest first, audio-only last. */
export function sortQualities(qs: Quality[]): Quality[] {
  const audio = (q: Quality) => Number(q.kind === "audio" || q.kind === "ytdlp-audio");
  return [...qs].sort((a, b) => audio(a) - audio(b) || (b.height ?? 0) - (a.height ?? 0) || b.bandwidth - a.bandwidth);
}

/** Is a media element big and visible enough to deserve the floating button (≥120×80, on screen)? */
export function eligibleRect(r: { width: number; height: number; top: number; bottom: number; left: number; right: number }, vw: number, vh: number): boolean {
  return r.width >= 120 && r.height >= 80 && r.bottom > 0 && r.right > 0 && r.top < vh && r.left < vw;
}

/** Chrome proxy settings (chrome.proxy.settings value) → ProxyInfo payload for the daemon. */
export interface ProxyInfo {
  mode: "direct" | "system" | "fixed" | "pac" | "auto";
  url: string | null;
}

interface ProxyServerLike {
  scheme?: string;
  host?: string;
  port?: number;
}

export function proxyInfoFrom(value: unknown): ProxyInfo {
  const v = (value ?? {}) as { mode?: string; rules?: { singleProxy?: ProxyServerLike; proxyForHttps?: ProxyServerLike; proxyForHttp?: ProxyServerLike } };
  switch (v.mode) {
    case "direct":
      return { mode: "direct", url: null };
    case "fixed_servers": {
      const s = v.rules?.singleProxy ?? v.rules?.proxyForHttps ?? v.rules?.proxyForHttp;
      if (!s?.host) return { mode: "system", url: null };
      const scheme = s.scheme === "socks5" || s.scheme === "socks4" || s.scheme === "https" ? s.scheme : "http";
      const port = s.port ?? (scheme.startsWith("socks") ? 1080 : 8080);
      return { mode: "fixed", url: `${scheme}://${s.host}:${port}` };
    }
    case "pac_script":
      return { mode: "pac", url: null };
    case "auto_detect":
      return { mode: "auto", url: null };
    default:
      return { mode: "system", url: null };
  }
}
