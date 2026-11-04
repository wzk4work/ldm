// Optional page-world hook (off by default; popup → 高级 → 页面钩子). Runs in the page's own
// JavaScript world (all frames, document_start). It reports playlist URLs requested through
// fetch / XMLHttpRequest (covers players that load manifests the webRequest sniffer cannot see)
// and observes Media Source Extensions: SourceBuffer codecs, append counts/byte totals and which
// <video> plays which MediaSource (shared/msehook.ts). It only sends URLs and counters to the LDM
// content script over a private extension channel (shared/bridge.ts); it never changes
// requests, responses or media data.
import { mainChannel } from "./shared/bridge";
import { installPageHooks } from "./shared/msehook";

if (typeof window !== "undefined") installPageHooks(window, mainChannel(window));
