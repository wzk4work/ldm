// yt-dlp sites (original code, MIT). LDM itself never extracts YouTube streams: on these pages the
// page URL is offered as one "yt-dlp" candidate and the daemon hands it to the user's yt-dlp.
import type { RuleSet } from "./protocol";
import { hostListed, hostOf } from "./policy";

export const YOUTUBE_HOSTS = ["youtube.com", "youtu.be", "youtube-nocookie.com"];

// Store build (`npm run build:store` / LDM_STORE_BUILD=1): every yt-dlp / YouTube page rule is
// dropped. No yt-dlp candidates, buttons or menu entries, and nothing is offered on YouTube pages.
declare const __LDM_STORE_BUILD__: boolean | undefined;
export const STORE_BUILD: boolean = typeof __LDM_STORE_BUILD__ === "boolean" ? __LDM_STORE_BUILD__ : false;

/** Store build only: pages on which the extension offers nothing (YouTube). */
export function storeExcluded(url: string | undefined | null, store: boolean = STORE_BUILD): boolean {
  if (!store || !url) return false;
  try {
    return hostListed(new URL(url).hostname.toLowerCase(), YOUTUBE_HOSTS);
  } catch {
    return false;
  }
}

/** yt-dlp is usable and not switched off for this site (`ytdlp.site_rules = off`). */
export function ytdlpAvailable(url: string | undefined | null, rules: Partial<RuleSet>, store: boolean = STORE_BUILD): boolean {
  if (store || !rules.ytdlp || !url || !/^https?:/i.test(url)) return false;
  return !hostListed(hostOf(url), rules.ytdlpOff ?? []);
}

function isYoutubeHost(h: string): boolean {
  return hostListed(h, YOUTUBE_HOSTS);
}

/** The canonical video URL when `url` is a video page of a yt-dlp-routed site, else null.
 *  YouTube: watch / shorts / live / embed / youtu.be pages (playlist and channel pages are not). */
export function ytdlpPageUrl(url: string | undefined | null, rules: Partial<RuleSet>, store: boolean = STORE_BUILD): string | null {
  if (!url || !ytdlpAvailable(url, rules, store)) return null;
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    return null;
  }
  const h = u.hostname.toLowerCase();
  const sites = rules.ytdlpSites ?? YOUTUBE_HOSTS;
  if (!hostListed(h, sites)) return null;
  if (!isYoutubeHost(h)) {
    u.hash = "";
    return u.href;
  }
  const id = (s: string | null | undefined) => (s && /^[\w-]{6,20}$/.test(s) ? s : null);
  if (h === "youtu.be" || h.endsWith(".youtu.be")) {
    const v = id(u.pathname.split("/")[1]);
    return v ? `https://www.youtube.com/watch?v=${v}` : null;
  }
  const music = h.startsWith("music.");
  const base = music ? "https://music.youtube.com" : "https://www.youtube.com";
  if (u.pathname === "/watch") {
    const v = id(u.searchParams.get("v"));
    return v ? `${base}/watch?v=${v}` : null;
  }
  const m = /^\/(shorts|live|embed)\/([\w-]{6,20})/.exec(u.pathname);
  if (m) return m[1] === "shorts" ? `https://www.youtube.com/shorts/${m[2]}` : `https://www.youtube.com/watch?v=${m[2]}`;
  return null;
}
