// Toolbar popup: host status, media list of the current tab with a quality picker, per-tab and per-site switches.
import { menuEntries, uiLang } from "./shared/adscore";
import { qualityLabel, sortQualities, type Quality } from "./shared/policy";

interface PopupState {
  items: { url: string; kind: string; size?: number | null; ad?: boolean; adScore?: number | null; adReasons?: string[] }[];
  hostReady: boolean;
  lastError?: string;
  settings: { intercept: boolean; debug?: boolean; pageHooks?: boolean };
  tabPaused: boolean;
  site: string;
  siteDisabled: boolean;
  ytdlp?: boolean;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const msg = (k: string, d = k) => chrome.i18n.getMessage(k) || d;
/** runtime.sendMessage that never leaves an unhandled rejection: Firefox rejects pending
 *  messages when the popup closes (or the background reloads) and logs them as uncaught. */
const send = (m: Record<string, unknown>): Promise<any> => chrome.runtime.sendMessage(m).catch(() => null);

function i18n(): void {
  for (const el of document.querySelectorAll<HTMLElement>("[data-i18n]")) el.textContent = msg(el.dataset.i18n!);
}

let tabId: number | undefined;

async function qualities(url: string, sel: HTMLSelectElement): Promise<void> {
  sel.textContent = "";
  const best = new Option(msg("qualityBest", "Best quality"), "");
  sel.add(best);
  const r = await send({ type: "ProbeMedia", url, tabId });
  if (!r?.ok) return;
  const seen = new Set<string>();
  for (const q of sortQualities((r.qualities ?? []) as Quality[])) {
    const text = qualityLabel(q, msg("qualityAudioOnly", "Audio only"));
    if (seen.has(text)) continue;
    seen.add(text);
    sel.add(new Option(text, q.kind === "audio" ? "0" : String(q.height ?? "")));
  }
}

async function refresh(): Promise<void> {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  tabId = tab?.id;
  const st: PopupState | null = await send({ type: "GetTabMedia", tabId });
  if (!st) return;
  const s = $("status");
  s.textContent = st.hostReady ? msg("hostConnected") : `${msg("hostDisconnected")} ${st.lastError ?? ""}`;
  s.className = "status " + (st.hostReady ? "ok" : "bad");
  ($("intercept") as HTMLInputElement).checked = st.settings.intercept;
  ($("pageHooks") as HTMLInputElement).checked = !!st.settings.pageHooks;
  ($("tabPaused") as HTMLInputElement).checked = st.tabPaused;
  ($("siteDisabled") as HTMLInputElement).checked = st.siteDisabled;
  $("site").textContent = st.site || "";
  const list = $("list");
  list.textContent = "";
  $("empty").style.display = st.items.length ? "none" : "block";
  for (const row of menuEntries(st.items, uiLang())) {
    const it = row.item;
    const li = document.createElement("li");
    if (row.greyed) li.className = "adish";
    else if (row.sub) li.className = "alt";
    const top = document.createElement("div");
    top.className = "top";
    const name = document.createElement("span");
    if (row.tag) {
      const tag = document.createElement("span");
      tag.className = "adtag";
      tag.textContent = row.tag;
      name.appendChild(tag);
    }
    const what = it.kind === "ytdlp" ? msg("popupYtdlpPage", "This page's video") : it.url.split(/[?#]/)[0].split("/").pop();
    name.appendChild(document.createTextNode(`${it.kind === "progressive" ? "" : "[" + (it.kind === "ytdlp" ? "yt-dlp" : it.kind.toUpperCase()) + "] "}${what}`));
    name.title = row.tooltip;
    const b = document.createElement("button");
    b.textContent = msg("popupDownload", "Download");
    top.append(name, b);
    li.appendChild(top);
    let sel: HTMLSelectElement | null = null;
    if (it.kind !== "progressive" && st.hostReady) {
      sel = document.createElement("select");
      li.appendChild(sel);
      void qualities(it.url, sel);
    }
    b.onclick = () => {
      const h = sel?.value ? Number(sel.value) : null;
      send({ type: "UserDownloadMedia", url: it.url, tabId, height: h }).then(() => window.close());
    };
    list.appendChild(li);
  }
  // always available: hand the page to yt-dlp (the "try yt-dlp" entry when LDM found nothing)
  const yb = $("ytdlp") as HTMLButtonElement;
  yb.style.display = st.ytdlp && st.hostReady && !st.items.some((i) => i.kind === "ytdlp") ? "block" : "none";
  yb.textContent = st.items.length ? msg("menuDownloadYtdlp", "Download with yt-dlp") : msg("popupTryYtdlp", "Nothing detected – try yt-dlp");
  yb.onclick = () => send({ type: "UserDownloadYtdlp", tabId }).then(() => window.close());
  if (st.settings.debug) {
    $("debug").style.display = "block";
    $("debugText").textContent = JSON.stringify(st.items, null, 1);
  }
}

i18n();
($("pageHooks") as HTMLInputElement).onchange = (e) =>
  send({ type: "SetSettings", settings: { pageHooks: (e.target as HTMLInputElement).checked } });
($("intercept") as HTMLInputElement).onchange = (e) => send({ type: "SetSettings", settings: { intercept: (e.target as HTMLInputElement).checked } });
($("tabPaused") as HTMLInputElement).onchange = (e) => send({ type: "SetTabPaused", tabId, paused: (e.target as HTMLInputElement).checked });
($("siteDisabled") as HTMLInputElement).onchange = (e) =>
  send({ type: "SetSiteDisabled", tabId, site: $("site").textContent, disabled: (e.target as HTMLInputElement).checked });
$("proxySync").onclick = () =>
  chrome.permissions
    .request({ permissions: ["proxy"] })
    .then((ok) => {
      $("proxySync").textContent = chrome.i18n.getMessage(ok ? "popupProxySynced" : "popupProxySync");
    })
    .catch(() => undefined);
void chrome.permissions
  .contains({ permissions: ["proxy"] })
  .then((ok) => {
    if (ok) $("proxySync").textContent = chrome.i18n.getMessage("popupProxySynced");
  })
  .catch(() => undefined);
$("reconnect").onclick = () => send({ type: "Reconnect" }).then(() => setTimeout(() => void refresh().catch(() => undefined), 800));
void refresh().catch(() => undefined);
