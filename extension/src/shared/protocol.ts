// LDM envelope protocol. Original code, MIT.

export const PROTOCOL_VERSION = 1;
export const NATIVE_HOST_NAME = "io.github.wzk4work.ldm.browser_host";
export const WS_SUBPROTOCOL = "ldm.bridge.v1";

export interface Envelope<P = Record<string, unknown>> {
  v: number;
  id: number;
  type: string;
  payload: P;
}

export interface HeaderKv {
  name: string;
  value: string;
}

export interface DownloadAdd {
  url: string;
  referrer?: string | null;
  page_url?: string | null;
  cookies?: string | null;
  user_agent?: string | null;
  mime?: string | null;
  filename_hint?: string | null;
  /** Page/video title of a media candidate: the daemon names the file after it (ahead of Content-Disposition). */
  title_hint?: string | null;
  preferred_height?: number | null;
  headers_extra?: HeaderKv[];
  bytes_total?: number | null;
  request_id?: string | null;
  tab_id?: number | null;
  kind?: "http" | "hls" | "dash" | "ytdlp" | null;
  source?: string;
  description?: string | null;
  /** separate audio file merged with `url` (video-only track file) */
  audio_url?: string | null;
  /** reasons when the user downloads a candidate ranked as a suspected ad */
  ad_reasons?: string[] | null;
}

/** `ytdlp`: the page itself, downloaded by the user's yt-dlp (YouTube, `always` sites). */
export type MediaKind = "progressive" | "hls" | "dash" | "ytdlp";

export interface MediaCandidate {
  tab_id: number;
  frame_id?: number | null;
  url: string;
  mime?: string | null;
  kind: MediaKind;
  page_url?: string | null;
  content_length?: number | null;
  filename_hint?: string | null;
  /** ad ranking (see shared/adscore.ts) */
  ad_score?: number | null;
  ad_suspect?: boolean;
  ad_reasons?: string[];
}

export interface InterceptDecision {
  action: "take" | "pass";
  jobId?: string | null;
  savePath?: string | null;
  requestId?: string | null;
  reason?: string | null;
}

export interface RuleSet {
  version: number;
  captureExts: string[];
  mediaExts: string[];
  excludeHosts: string[];
  excludeUrlPatterns: string[];
  minSize: number;
  captureEnabled: boolean;
  siteAdapters?: unknown[];
  /** held while clicking a link: let the browser keep the download */
  bypassModifier?: string;
  /** held while clicking a link: LDM takes it even if the type is not in the list */
  forceModifier?: string;
  /** media.ad_ranking */
  adRanking?: boolean;
  /** domains from media.ad_domains_file */
  adDomains?: string[];
  /** yt-dlp usable on the daemon side (enabled + binary found) */
  ytdlp?: boolean;
  /** sites routed to yt-dlp (YouTube + `ytdlp.site_rules = always`) */
  ytdlpSites?: string[];
  /** sites where yt-dlp is never offered (`off`) */
  ytdlpOff?: string[];
  /** ytdlp.auto_fallback: a page-initiated download with nothing LDM can fetch goes to yt-dlp */
  ytdlpAutoFallback?: boolean;
}

export const DEFAULT_RULES: RuleSet = {
  version: 0,
  captureExts: [
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "mp3", "m4a", "aac", "ogg", "opus", "flac", "wav",
    "zip", "7z", "rar", "gz", "tar", "xz", "bz2", "tgz", "zst", "iso", "exe", "msi", "deb", "rpm", "appimage", "apk", "dmg", "pdf", "epub",
  ],
  mediaExts: ["mp4", "m4v", "webm", "mkv", "mov", "flv", "mp3", "m4a", "aac", "ogg", "opus", "m3u8", "mpd"],
  excludeHosts: [],
  excludeUrlPatterns: [],
  minSize: 10240,
  captureEnabled: true,
  bypassModifier: "alt",
  forceModifier: "insert",
};

let nextId = 1;

export function envelope<P extends object>(type: string, payload: P, id?: number): Envelope<P> {
  return { v: PROTOCOL_VERSION, id: id ?? nextId++, type, payload };
}

/** Validate an inbound envelope (v, id, type, payload object). */
export function isEnvelope(x: unknown): x is Envelope {
  if (typeof x !== "object" || x === null) return false;
  const e = x as Record<string, unknown>;
  return (
    e.v === PROTOCOL_VERSION &&
    typeof e.id === "number" &&
    typeof e.type === "string" &&
    e.type.length > 0 &&
    typeof e.payload === "object" &&
    e.payload !== null &&
    !Array.isArray(e.payload)
  );
}
