// Context menus.
import { STORE_BUILD } from "./ytdlp";
export const MENU = {
  link: "ldm-link",
  media: "ldm-media",
  allLinks: "ldm-all-links",
  selection: "ldm-selection",
  ytdlp: "ldm-ytdlp",
} as const;

export function createMenus(): void {
  chrome.contextMenus.removeAll(() => {
    const t = (k: string) => chrome.i18n.getMessage(k) || k;
    chrome.contextMenus.create({ id: MENU.link, title: t("menuDownloadLink"), contexts: ["link"] });
    chrome.contextMenus.create({ id: MENU.media, title: t("menuDownloadMedia"), contexts: ["video", "audio"] });
    chrome.contextMenus.create({ id: MENU.allLinks, title: t("menuDownloadAll"), contexts: ["page"] });
    chrome.contextMenus.create({ id: MENU.selection, title: t("menuDownloadSelection"), contexts: ["selection"] });
    // always offered (except in the store build): the page (or the right-clicked link) goes to the user's yt-dlp
    if (!STORE_BUILD) chrome.contextMenus.create({ id: MENU.ytdlp, title: t("menuDownloadYtdlp"), contexts: ["page", "link", "video", "audio", "frame"] });
  });
}

/** Extract http(s) URLs from free text (selection download). */
export function urlsInText(text: string): string[] {
  const out = new Set<string>();
  for (const m of text.matchAll(/https?:\/\/[^\s"'<>]+/gi)) out.add(m[0].replace(/[),.;]+$/, ""));
  return [...out];
}
