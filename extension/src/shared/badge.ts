// Toolbar badge: media count per tab, red "!" when the host is unreachable.
export function setMediaCount(tabId: number, n: number): void {
  chrome.action.setBadgeBackgroundColor({ color: "#1a73e8", tabId }).catch(() => undefined);
  chrome.action.setBadgeText({ text: n > 0 ? String(Math.min(n, 99)) : "", tabId }).catch(() => undefined);
}

export function setHostStatus(up: boolean, via?: string): void {
  chrome.action.setTitle({ title: up ? `LDM: ${chrome.i18n.getMessage("hostConnected")} (${via ?? ""})` : `LDM: ${chrome.i18n.getMessage("hostDisconnected")}` }).catch(() => undefined);
  if (!up) {
    chrome.action.setBadgeBackgroundColor({ color: "#d93025" }).catch(() => undefined);
    chrome.action.setBadgeText({ text: "!" }).catch(() => undefined);
  } else {
    chrome.action.setBadgeText({ text: "" }).catch(() => undefined);
  }
}

/** Grey "off" badge on tabs where interception is paused (per-tab switch or disabled site). */
export function setTabDisabled(tabId: number, off: boolean, mediaCount = 0): void {
  if (off) {
    chrome.action.setBadgeBackgroundColor({ color: "#80868b", tabId }).catch(() => undefined);
    chrome.action.setBadgeText({ text: "off", tabId }).catch(() => undefined);
  } else {
    setMediaCount(tabId, mediaCount);
  }
}
