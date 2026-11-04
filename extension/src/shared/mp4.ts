// Minimal ISO-BMFF reader: which kind of track(s) a (fragmented) MP4 file holds, from its
// first bytes (moov → trak → mdia → hdlr). Used for sites that stream video and audio as two
// separate whole files: the file's own header says which one is the audio.

export type TrackType = "video" | "audio" | "muxed";

const CONTAINERS = new Set(["moov", "trak", "mdia", "mvex"]);

function fourcc(b: Uint8Array, o: number): string {
  return String.fromCharCode(b[o], b[o + 1], b[o + 2], b[o + 3]);
}

/** Handler types ("vide", "soun", …) of every track found in `b` (a file prefix is enough). */
export function mp4Handlers(b: Uint8Array): string[] {
  const out: string[] = [];
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const walk = (start: number, end: number, depth: number) => {
    let o = start;
    while (o + 8 <= end && depth < 6) {
      let size = dv.getUint32(o);
      const type = fourcc(b, o + 4);
      let head = 8;
      if (size === 1) {
        if (o + 16 > end) return;
        size = Number(dv.getBigUint64(o + 8));
        head = 16;
      } else if (size === 0) size = end - o;
      if (size < head) return;
      const boxEnd = Math.min(o + size, end);
      if (CONTAINERS.has(type)) walk(o + head, boxEnd, depth + 1);
      // hdlr: version/flags(4) pre_defined(4) handler_type(4)
      else if (type === "hdlr" && o + head + 12 <= boxEnd) out.push(fourcc(b, o + head + 8));
      if (o + size > end) return; // the prefix ends inside this box
      o += size;
    }
  };
  walk(0, b.byteLength, 0);
  return out;
}

/** "video", "audio", "muxed" (both) or null (not an MP4 header / no tracks in the prefix). */
export function mp4TrackType(b: Uint8Array): TrackType | null {
  const h = mp4Handlers(b);
  const v = h.includes("vide");
  const a = h.includes("soun");
  return v && a ? "muxed" : v ? "video" : a ? "audio" : null;
}

export interface Mp4Info {
  track: TrackType | null;
  /** seconds (movie header, or the fragmented file's mehd), null when unknown */
  duration: number | null;
  /** the video track's display size (tkhd), null when unknown */
  width: number | null;
  height: number | null;
}

/** Track type, duration and video size from an MP4 file prefix (a faststart / fragmented header). */
export function mp4Info(b: Uint8Array): Mp4Info {
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  let timescale = 0;
  let movieDur = 0;
  let fragDur = 0;
  let width: number | null = null;
  let height: number | null = null;
  const handlers: string[] = [];
  const num = (o: number, v1: boolean) => (v1 ? Number(dv.getBigUint64(o)) : dv.getUint32(o));
  // per trak: its handler and tkhd size
  let trak: { hdlr?: string; w?: number; h?: number } | null = null;
  const walk = (start: number, end: number, depth: number) => {
    let o = start;
    while (o + 8 <= end && depth < 6) {
      let size = dv.getUint32(o);
      const type = fourcc(b, o + 4);
      let head = 8;
      if (size === 1) {
        if (o + 16 > end) return;
        size = Number(dv.getBigUint64(o + 8));
        head = 16;
      } else if (size === 0) size = end - o;
      if (size < head) return;
      const boxEnd = Math.min(o + size, end);
      const p = o + head;
      const v1 = p < boxEnd && b[p] === 1;
      if (type === "trak") {
        trak = {};
        walk(p, boxEnd, depth + 1);
        const t = trak as { hdlr?: string; w?: number; h?: number };
        if (t.hdlr) handlers.push(t.hdlr);
        if (t.hdlr === "vide" && t.h && height == null) {
          width = t.w ?? null;
          height = t.h;
        }
        trak = null;
      } else if (CONTAINERS.has(type)) walk(p, boxEnd, depth + 1);
      else if (type === "hdlr" && p + 12 <= boxEnd) {
        const h = fourcc(b, p + 8);
        if (trak) (trak as { hdlr?: string }).hdlr = h;
        else handlers.push(h);
      } else if (type === "mvhd" && p + (v1 ? 32 : 20) <= boxEnd) {
        timescale = dv.getUint32(p + (v1 ? 20 : 12));
        movieDur = num(p + (v1 ? 24 : 16), v1);
      } else if (type === "mehd" && p + (v1 ? 12 : 8) <= boxEnd) fragDur = num(p + 4, v1);
      else if (type === "tkhd" && trak && o + size <= end && size >= head + 84) {
        // the last 8 bytes: width and height, 16.16 fixed point
        (trak as { w?: number }).w = Math.round(dv.getUint32(o + size - 8) / 65536);
        (trak as { h?: number }).h = Math.round(dv.getUint32(o + size - 4) / 65536);
      }
      if (o + size > end) return;
      o += size;
    }
  };
  walk(0, b.byteLength, 0);
  const v = handlers.includes("vide");
  const a = handlers.includes("soun");
  const ticks = movieDur || fragDur;
  return {
    track: v && a ? "muxed" : v ? "video" : a ? "audio" : null,
    duration: timescale > 0 && ticks > 0 && ticks < 0xffffffff ? Math.round((ticks / timescale) * 100) / 100 : null,
    width: height ? width : null,
    height: height || null,
  };
}
