// Filtering for the "download all links" picker page.
import { urlExt } from "./classify";

export interface PickLink {
  url: string;
  text?: string;
}

const PAGE_EXTS = new Set(["", "html", "htm", "php", "asp", "aspx", "jsp", "shtml"]);

/** type: "" = all, "files" = anything that is not a web page, otherwise an extension. */
export function filterLinks(links: PickLink[], type: string, text: string): PickLink[] {
  const q = text.trim().toLowerCase();
  return links.filter((l) => {
    const e = urlExt(l.url);
    if (type === "files" && PAGE_EXTS.has(e)) return false;
    if (type && type !== "files" && e !== type) return false;
    return !q || l.url.toLowerCase().includes(q) || (l.text ?? "").toLowerCase().includes(q);
  });
}
