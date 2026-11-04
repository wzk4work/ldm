// Page-title → file-name cleaning for media candidates (videos are named after the
// page's video title). Mirrors ldm-core `filename::clean_title`; the daemon cleans again,
// so both sides agree even when one is older.

const SEPS = [" - ", " | ", " – ", " — ", " · ", " • ", " :: ", " / ", "_"];

const KNOWN = [
  "youtube", "vimeo", "dailymotion", "bilibili", "哔哩哔哩", "twitter", "facebook", "instagram", "tiktok", "抖音",
  "twitch", "reddit", "pornhub", "pornhubcom", "xvideos", "xhamster", "youku", "优酷", "爱奇艺", "iqiyi", "腾讯视频",
  "西瓜视频", "niconico", "ニコニコ動画", "rumble", "odysee", "streamable", "ted", "archiveorg", "internetarchive",
];
const SLD = ["co", "com", "net", "org", "gov", "edu", "ac", "ne", "or"];
const BRAND_TAILS = ["com", "net", "org", "tv", "io", "cn", "jp", "co", "couk", "video", "videos", "official"];

const norm = (s: string): string => Array.from(s.toLowerCase()).filter((c) => /[\p{L}\p{N}]/u.test(c)).join("");

/** Registrable brand label: www.pornhub.com → pornhub, news.bbc.co.uk → bbc. */
export function hostBrand(host: string): string {
  const l = host.replace(/\.$/, "").split(".").filter(Boolean).map((x) => x.toLowerCase());
  if (l.length < 2) return l[0] ?? "";
  const n = l.length;
  return n >= 3 && l[n - 1].length === 2 && SLD.includes(l[n - 2]) ? l[n - 3] : l[n - 2];
}

/** Generic words that may precede a site name in a title suffix ("video Dailymotion", "Watch on Vimeo"). */
const BRAND_HEADS = ["video", "videos", "watchon", "on"];

function isBrand(seg: string, brand: string, hostNorm: string): boolean {
  const n = norm(seg);
  if (!n) return false;
  return BRAND_HEADS.some((h) => n.length > h.length && n.startsWith(h) && isBrandNorm(n.slice(h.length), brand, hostNorm)) || isBrandNorm(n, brand, hostNorm);
}

function isBrandNorm(n: string, brand: string, hostNorm: string): boolean {
  if (brand.length >= 2) {
    if (n === brand || n === hostNorm || (hostNorm.endsWith(n) && n.startsWith(brand))) return true;
    if (n.startsWith(brand) && BRAND_TAILS.includes(n.slice(brand.length))) return true;
  }
  const known = KNOWN.map(norm);
  return (
    known.some((k) => n === k || n === k + "com" || n === k + "tv") ||
    known.some((a) => n.startsWith(a) && known.some((b) => n.length === a.length + b.length && n.endsWith(b)))
  );
}

/** Strip notification counters and site names ("(3) Title - YouTube" → "Title"). */
export function cleanTitle(title: string | null | undefined, pageUrl?: string | null): string {
  let host = "";
  try {
    host = pageUrl ? new URL(pageUrl).hostname : "";
  } catch {
    host = "";
  }
  const brand = hostBrand(host);
  const hostNorm = norm(host);
  let t = (title ?? "").split(/\s+/).filter(Boolean).join(" ");
  for (;;) {
    const m = /^[([](\d+)\+?[)\]]\s*/.exec(t);
    if (!m) break;
    t = t.slice(m[0].length);
  }
  for (;;) {
    let cut = -1;
    for (const sep of SEPS) {
      const i = t.lastIndexOf(sep);
      if (i > 0 && isBrand(t.slice(i + sep.length), brand, hostNorm) && i > cut) cut = i;
    }
    if (cut < 0) break;
    t = t.slice(0, cut).trimEnd();
  }
  for (const sep of SEPS) {
    const i = t.indexOf(sep);
    if (i < 0) continue;
    const rest = t.slice(i + sep.length).trim();
    if (rest && isBrand(t.slice(0, i), brand, hostNorm)) {
      t = rest;
      break;
    }
  }
  t = t.trim();
  // "NASA on TikTok" (og:title style): drop a trailing " on <site>"
  const on = t.toLowerCase().lastIndexOf(" on ");
  if (on > 0 && isBrandNorm(norm(t.slice(on + 4)), brand, hostNorm)) t = t.slice(0, on).trimEnd();
  // nothing but the site name, or a login / sign-up dialog's title: useless as a file name
  return isBrand(t, brand, hostNorm) || AUTH_TITLES.includes(t.toLowerCase()) ? "" : t;
}

/** Tab titles that sites set while a login / sign-up dialog is open. */
const AUTH_TITLES = ["sign up", "signup", "log in", "login", "sign in", "signin", "log in or sign up", "sign up or log in", "登录", "注册", "登录/注册", "登录或注册"];

/** Title candidates read from the page itself (top frame). */
export interface PageTitles {
  og?: string | null;
  h1?: string | null;
}

/**
 * The video title for a media download: the cleaned tab title; when that is
 * only the site name (some sites ship `<title>Site</title>` and show the title in the page),
 * the page's og:title, then its first visible h1.
 */
export function pickTitle(tabTitle: string | null | undefined, pageUrl: string | null | undefined, alt?: PageTitles | null): string {
  for (const t of [tabTitle, alt?.og, alt?.h1]) {
    const c = cleanTitle(t, pageUrl);
    if (c) return c.length > 300 ? c.slice(0, 300) : c;
  }
  return "";
}
