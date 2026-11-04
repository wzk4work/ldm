// "Download all links" picker: tick links, filter by type or text, send the selection to LDM.
import { urlExt } from "./shared/classify";
import { filterLinks, type PickLink } from "./shared/picker";

interface Picker {
  page: string | null;
  title: string;
  links: PickLink[];
  preselect: string[];
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const msg = (k: string, d = k) => chrome.i18n.getMessage(k) || d;
const checked = new Set<string>();
let picker: Picker = { page: null, title: "", links: [], preselect: [] };

function visible(): PickLink[] {
  return filterLinks(picker.links, ($("type") as HTMLSelectElement).value, ($("filter") as HTMLInputElement).value);
}

function render(): void {
  const body = $("rows");
  body.textContent = "";
  for (const l of visible()) {
    const tr = document.createElement("tr");
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = checked.has(l.url);
    cb.onchange = () => {
      if (cb.checked) checked.add(l.url);
      else checked.delete(l.url);
      count();
    };
    const c0 = document.createElement("td");
    c0.appendChild(cb);
    const c1 = document.createElement("td");
    c1.className = "url";
    c1.textContent = decodeURIComponent(l.url.split(/[?#]/)[0].split("/").pop() || l.url);
    c1.title = l.url;
    const c2 = document.createElement("td");
    c2.textContent = l.text ?? "";
    const c3 = document.createElement("td");
    c3.textContent = urlExt(l.url) || "-";
    tr.append(c0, c1, c2, c3);
    body.appendChild(tr);
  }
  count();
}

function count(): void {
  $("count").textContent = `${checked.size} / ${picker.links.length}`;
}

async function main(): Promise<void> {
  for (const el of document.querySelectorAll<HTMLElement>("[data-i18n]")) el.textContent = msg(el.dataset.i18n!);
  ($("filter") as HTMLInputElement).placeholder = msg("linksFilter", "filter");
  picker = ((await chrome.runtime.sendMessage({ type: "GetLinkPicker" }).catch(() => null)) as Picker) ?? picker;
  $("page").textContent = `${picker.title} — ${picker.page ?? ""}`;
  for (const u of picker.preselect) checked.add(u);
  const types = new Map<string, string>([["", msg("linksTypeAll", "all")], ["files", msg("linksTypeFiles", "files")]]);
  for (const l of picker.links) {
    const e = urlExt(l.url);
    if (e) types.set(e, "." + e);
  }
  const sel = $("type") as HTMLSelectElement;
  for (const [v, t] of types) sel.add(new Option(t, v));
  sel.onchange = render;
  ($("filter") as HTMLInputElement).oninput = render;
  $("all").onclick = () => {
    for (const l of visible()) checked.add(l.url);
    render();
  };
  $("none").onclick = () => {
    for (const l of visible()) checked.delete(l.url);
    render();
  };
  $("go").onclick = async () => {
    const urls = picker.links.map((l) => l.url).filter((u) => checked.has(u));
    const r = await chrome.runtime.sendMessage({ type: "DownloadLinks", urls, page: picker.page }).catch((e: unknown) => ({ ok: false, error: String((e as Error)?.message ?? e) }));
    $("count").textContent = r?.ok ? `✓ ${r.count}` : String(r?.error ?? "error");
    if (r?.ok) setTimeout(() => window.close(), 800);
  };
  render();
}

void main();
