// Per-tab media candidate aggregation.
import type { MediaCandidate, MediaKind } from "./protocol";
import { codecsFor, groupedOrder, qualityAlternates, rankOrder, scoreAll, type PageSignals, type RankOptions } from "./adscore";

export interface TabMedia {
  url: string;
  kind: MediaKind;
  mime?: string | null;
  size?: number | null;
  title?: string | null;
  lastSeen: number;
  /** first time the URL was seen on the tab (ms) */
  firstSeen?: number;
  /** duration in seconds, when known (e.g. from the HLS playlist) */
  duration?: number | null;
  /** a whole single-track file (video-only / audio-only fMP4 fetched with Range requests) */
  track?: "video" | "audio" | "muxed" | null;
  /** video-only track for which the page also has an audio track (downloaded and merged together) */
  paired?: boolean;
  /** fetched once and found not to be a playlist ("vast" = VAST/VMAP ad XML) */
  notPlaylist?: "vast" | "other" | null;
  /** ad ranking (filled by list()) */
  adScore?: number;
  ad?: boolean;
  adReasons?: string[];
  /** variant playlists folded into this master (filled by list()) */
  related?: string[];
  /** codecs of the MSE players fed from it (page hook; filled by list()) */
  codecs?: string[];
  /** video height from the file's MP4 header */
  height?: number | null;
  /** another bitrate of the same video: URL of the main entry (filled by list()) */
  altOf?: string | null;
}

/** Ad ranking settings pushed by the daemon (media.ad_ranking / media.ad_domains_file). */
export interface AdRankSettings {
  enabled: boolean;
  userDomains: string[];
  builtinDomains?: string[];
}

const KIND_RANK: Record<MediaKind, number> = { progressive: 0, dash: 1, hls: 1, ytdlp: 2 };

/** Dedupe key: URL without #fragment. */
export function dedupeKey(url: string): string {
  const i = url.indexOf("#");
  return i >= 0 ? url.slice(0, i) : url;
}

/**
 * URIs referenced by an HLS master playlist (variant streams and EXT-X-MEDIA renditions),
 * resolved against the playlist URL. Returns null when the text is not a master playlist.
 */
/** Total duration of an HLS media playlist (sum of #EXTINF), null for masters and live lists. */
/** What a fetched "playlist" body really is: null = a playlist, "vast" = VAST/VMAP ad XML, "other" = anything else. */
export function playlistBodyKind(text: string): "vast" | "other" | null {
  const t = text.replace(/^\uFEFF/, "").trimStart();
  if (t.startsWith("#EXTM3U")) return null;
  if (/^<\?xml[^>]*>\s*(<!--[\s\S]*?-->\s*)*<(\w+:)?(VAST|VMAP)\b/i.test(t) || /^<(\w+:)?(VAST|VMAP)\b/i.test(t)) return "vast";
  return "other";
}

export function hlsDuration(text: string): number | null {
  if (!text.trimStart().startsWith("#EXTM3U") || /#EXT-X-STREAM-INF/i.test(text) || !/#EXT-X-ENDLIST/i.test(text)) return null;
  let total = 0;
  for (const m of text.matchAll(/#EXTINF:\s*([\d.]+)/gi)) total += Number(m[1]) || 0;
  return total > 0 ? Math.round(total * 100) / 100 : null;
}

/** First variant URI of a master playlist (resolved), for a duration lookup. */
export function hlsFirstVariant(text: string, base: string): string | null {
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    if (!/^#EXT-X-STREAM-INF/i.test(lines[i].trim())) continue;
    for (let j = i + 1; j < lines.length; j++) {
      const n = lines[j].trim();
      if (!n || n.startsWith("#")) continue;
      try {
        return new URL(n, base).href;
      } catch {
        return null;
      }
    }
  }
  return null;
}

export function hlsChildUris(text: string, base: string): string[] | null {
  if (!text.trimStart().startsWith("#EXTM3U") || !/#EXT-X-STREAM-INF/i.test(text)) return null;
  const out: string[] = [];
  const abs = (u: string) => {
    try {
      out.push(dedupeKey(new URL(u.trim(), base).href));
    } catch {
      /* ignore malformed */
    }
  };
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const l = lines[i].trim();
    if (/^#EXT-X-(STREAM-INF|I-FRAME-STREAM-INF|MEDIA)[:]/i.test(l)) {
      const m = /URI="([^"]+)"/i.exec(l);
      if (m) abs(m[1]);
      if (/^#EXT-X-STREAM-INF/i.test(l)) {
        // the variant URI is the next non-comment line
        for (let j = i + 1; j < lines.length; j++) {
          const n = lines[j].trim();
          if (!n || n.startsWith("#")) continue;
          abs(n);
          break;
        }
      }
    }
  }
  return out;
}

export class MediaRegistry {
  private tabs = new Map<number, Map<string, TabMedia>>();
  /** per tab: playlists that belong to a detected HLS master (hidden from the list) */
  private children = new Map<number, Map<string, string>>();
  private inspected = new Set<string>();
  private signals = new Map<number, PageSignals>();
  private frames = new Map<number, Map<number, PageSignals>>();
  adSettings: AdRankSettings = { enabled: true, userDomains: [] };
  constructor(private ttlMs = 10 * 60 * 1000, private maxPerTab = 50) {}

  /** Returns true when the entry is new or became more specific. */
  add(tabId: number, item: Omit<TabMedia, "lastSeen">, now = Date.now()): boolean {
    let m = this.tabs.get(tabId);
    if (!m) {
      m = new Map();
      this.tabs.set(tabId, m);
    }
    const key = dedupeKey(item.url);
    const old = m.get(key);
    if (old) {
      let changed = false;
      if (KIND_RANK[item.kind] > KIND_RANK[old.kind]) {
        old.kind = item.kind;
        changed = true;
      }
      if (!old.mime && item.mime) {
        old.mime = item.mime;
        changed = true;
      }
      if (old.size == null && item.size != null) {
        old.size = item.size;
        changed = true;
      }
      old.lastSeen = now;
      return changed;
    }
    m.set(key, { ...item, url: key, lastSeen: now, firstSeen: now });
    if (m.size > this.maxPerTab) {
      const oldest = [...m.values()].sort((a, b) => a.lastSeen - b.lastSeen)[0];
      m.delete(oldest.url);
    }
    return true;
  }

  list(tabId: number, now = Date.now()): TabMedia[] {
    const m = this.tabs.get(tabId);
    if (!m) return [];
    // the yt-dlp page entry lives as long as the page (it is not network media that can expire)
    for (const [k, v] of m) if (v.kind !== "ytdlp" && now - v.lastSeen > this.ttlMs) m.delete(k);
    const kids = this.children.get(tabId);
    // a variant / rendition playlist is listed only through its master (as a quality)
    const visible = [...m.values()].filter((v) => {
      const master = kids?.get(v.url);
      return !(master && m.has(master));
    });
    // manifests first (they are what users want), then by recency …
    visible.sort((a, b) => KIND_RANK[b.kind] - KIND_RANK[a.kind] || b.lastSeen - a.lastSeen);
    // … then the ad ranking: likely main video first, suspected ads greyed at the bottom
    for (const v of visible) v.related = kids ? [...kids].filter(([, master]) => master === v.url).map(([child]) => child) : [];
    const opts: RankOptions = { ...this.adSettings, page: this.signals.get(tabId) ?? null };
    const verdicts = scoreAll(visible, opts);
    const hasAudio = visible.some((v) => v.track === "audio");
    // the same video at several bitrates: one main entry, the others listed right under it
    const alt = opts.enabled === false ? visible.map(() => null) : qualityAlternates(visible, verdicts, opts.page);
    return groupedOrder(rankOrder(verdicts), alt).map((i) => {
      const altOf = alt[i] != null ? visible[alt[i]!].url : null;
      const adReasons = altOf ? [...verdicts[i].reasons, `alt-of:${altOf}`] : verdicts[i].reasons;
      return Object.assign(visible[i], { adScore: verdicts[i].score, ad: verdicts[i].ad, adReasons, altOf, codecs: codecsFor(visible[i], opts.page), paired: visible[i].track === "video" && hasAudio });
    });
  }

  /** Page-side signals from one frame's content script (players, skip-ad controls, declared duration). */
  setSignals(tabId: number, s: PageSignals, frameId = 0): void {
    let f = this.frames.get(tabId);
    if (!f) {
      f = new Map();
      this.frames.set(tabId, f);
    }
    f.set(frameId, { declaredDuration: s.declaredDuration ?? null, players: (s.players ?? []).slice(-30).map((p) => (p.el ? { ...p, el: `${frameId}:${p.el}` } : p)), skipWindows: (s.skipWindows ?? []).slice(-20) });
    const all = [...f.entries()];
    const top = f.get(0);
    const cur = this.signals.get(tabId) ?? {};
    this.signals.set(tabId, {
      declaredDuration: top?.declaredDuration ?? all.map(([, x]) => x.declaredDuration).find((d) => d != null) ?? null,
      players: all.flatMap(([, x]) => x.players ?? []),
      skipWindows: all.flatMap(([, x]) => x.skipWindows ?? []),
      adRequests: cur.adRequests ?? [],
    });
  }

  /** A VAST / VMAP / IMA request from the tab. */
  noteAdRequest(tabId: number, at = Date.now()): void {
    const s = this.signals.get(tabId) ?? {};
    s.adRequests = [...(s.adRequests ?? []), at].slice(-20);
    this.signals.set(tabId, s);
  }

  /** Duration of a playlist (seconds), learnt by inspecting it. */
  setDuration(tabId: number, url: string, secs: number | null): boolean {
    const it = this.tabs.get(tabId)?.get(dedupeKey(url));
    if (!it || secs == null || it.duration === secs) return false;
    it.duration = secs;
    return true;
  }

  /** Duration and video height read from a progressive file's own MP4 header. */
  setMp4Info(tabId: number, url: string, info: { duration?: number | null; height?: number | null }): boolean {
    const it = this.tabs.get(tabId)?.get(dedupeKey(url));
    if (!it) return false;
    let changed = false;
    if (info.duration != null && info.duration > 0 && it.duration == null) {
      it.duration = Math.round(info.duration * 100) / 100;
      changed = true;
    }
    if (info.height != null && info.height > 0 && it.height !== info.height) {
      it.height = info.height;
      changed = true;
    }
    return changed;
  }

  /** Track type read from the file's own MP4 header. */
  setTrack(tabId: number, url: string, t: "video" | "audio" | "muxed" | null): boolean {
    const it = this.tabs.get(tabId)?.get(dedupeKey(url));
    if (!it || t == null || it.track === t) return false;
    it.track = t;
    return true;
  }

  /** The audio file that goes with a video-only track on this tab (most recently used one). */
  audioTrackFor(tabId: number, now = Date.now()): string | null {
    const a = this.list(tabId, now).filter((i) => i.track === "audio");
    a.sort((x, y) => y.lastSeen - x.lastSeen);
    return a[0]?.url ?? null;
  }

  /** The "playlist" turned out not to be one (e.g. a VAST/VMAP ad description served as .m3u8). */
  setNotPlaylist(tabId: number, url: string, what: "vast" | "other"): boolean {
    const it = this.tabs.get(tabId)?.get(dedupeKey(url));
    if (!it || it.notPlaylist === what) return false;
    it.notPlaylist = what;
    return true;
  }

  /** Signature of the visible order, ad flags and scores (to push the page button only when its rows or tooltips change). */
  rankSignature(tabId: number): string {
    return this.list(tabId).map((i) => `${i.url}|${i.ad ? 1 : 0}|${i.adScore ?? ""}|${(i.codecs ?? []).join(",")}|${i.altOf ?? ""}`).join(" ");
  }

  /** Should this HLS playlist be fetched once to learn whether it is a master? */
  needsInspection(tabId: number, url: string): boolean {
    const k = `${tabId} ${dedupeKey(url)}`;
    if (this.inspected.has(k) || this.children.get(tabId)?.has(dedupeKey(url))) return false;
    this.inspected.add(k);
    if (this.inspected.size > 2000) this.inspected.clear();
    return true;
  }

  /** Record the child playlists of a master; returns true when the visible list changed. */
  setChildren(tabId: number, masterUrl: string, uris: string[]): boolean {
    let kids = this.children.get(tabId);
    if (!kids) {
      kids = new Map();
      this.children.set(tabId, kids);
    }
    const master = dedupeKey(masterUrl);
    let changed = false;
    for (const u of uris) {
      if (u === master || kids.has(u)) continue;
      kids.set(u, master);
      if (this.tabs.get(tabId)?.has(u)) changed = true;
    }
    return changed;
  }

  /** Is there a manifest on this tab (so its segments are redundant)? */
  hasManifest(tabId: number, now = Date.now()): boolean {
    return this.list(tabId, now).some((x) => x.kind !== "progressive");
  }

  /** Drop one entry (e.g. the yt-dlp page entry after an in-page navigation). */
  remove(tabId: number, url: string): boolean {
    return this.tabs.get(tabId)?.delete(url) ?? false;
  }

  clear(tabId: number): void {
    this.tabs.delete(tabId);
    this.children.delete(tabId);
    this.signals.delete(tabId);
    this.frames.delete(tabId);
  }

  toCandidate(tabId: number, it: TabMedia, pageUrl?: string | null): MediaCandidate {
    return {
      tab_id: tabId, url: it.url, kind: it.kind, mime: it.mime ?? null, page_url: pageUrl ?? null, content_length: it.size ?? null, filename_hint: it.title ?? null,
      ad_score: it.adScore ?? null, ad_suspect: it.ad ?? false, ad_reasons: it.adReasons ?? [],
    };
  }
}
