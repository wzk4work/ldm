// URL / MIME classification.
import type { MediaKind, RuleSet } from "./protocol";

export type Verdict = "ignore" | "file" | "media" | "manifest" | "segment";

const MEDIA_MIME = [/^video\//i, /^audio\//i];
const MANIFEST_MIME = [/mpegurl/i, /dash\+xml/i];
const SEGMENT_EXT = new Set(["ts", "m4s", "m4f", "aac"]);

export function urlExt(url: string): string {
  let path = url;
  try {
    path = new URL(url).pathname;
  } catch {
    path = url.split(/[?#]/)[0];
  }
  const last = path.split("/").pop() ?? "";
  const dot = last.lastIndexOf(".");
  return dot > 0 ? last.slice(dot + 1).toLowerCase() : "";
}

export function classify(url: string, mime?: string | null, disposition?: string | null, rules?: RuleSet): Verdict {
  const ext = urlExt(url);
  const m = (mime ?? "").split(";")[0].trim();
  if (ext === "m3u8" || ext === "mpd" || MANIFEST_MIME.some((r) => r.test(m))) return "manifest";
  if (SEGMENT_EXT.has(ext) && ext !== "aac") return "segment";
  if (m === "video/mp2t" || m === "video/iso.segment") return "segment";
  if (MEDIA_MIME.some((r) => r.test(m))) return "media";
  const mediaExts = rules?.mediaExts ?? ["mp4", "m4v", "webm", "mkv", "mov", "flv", "mp3", "m4a", "ogg", "opus"];
  if (mediaExts.includes(ext)) return "media";
  if (disposition && /attachment/i.test(disposition)) return "file";
  if (rules?.captureExts.includes(ext)) return "file";
  return "ignore";
}

export function mediaKind(url: string, mime?: string | null): MediaKind {
  const v = classify(url, mime);
  if (v === "manifest") {
    const ext = urlExt(url);
    if (ext === "mpd" || /dash/i.test(mime ?? "")) return "dash";
    return "hls";
  }
  return "progressive";
}

/** Filename from a Content-Disposition header (RFC 6266 subset). */
export function dispositionFilename(v?: string | null): string | null {
  if (!v) return null;
  const star = /filename\*\s*=\s*([^']*)'[^']*'([^;]+)/i.exec(v);
  if (star) {
    try {
      return decodeURIComponent(star[2].trim().replace(/^"|"$/g, ""));
    } catch {
      /* fall through */
    }
  }
  const plain = /filename\s*=\s*("([^"]*)"|[^;]+)/i.exec(v);
  if (plain) return (plain[2] ?? plain[1]).trim();
  return null;
}
