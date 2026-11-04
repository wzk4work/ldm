// ISOLATED-world half of the private page-hook channel (registered together with pagehook.js,
// document_start, all frames; only while page hooks are switched on). It completes the
// handshake before any page script runs and buffers reports for the content script, which is
// injected later (document_idle). See shared/bridge.ts.
import { hookSink, isolatedChannel, MAX_QUEUED } from "./shared/bridge";

const sink = hookSink(globalThis as unknown as Record<string, unknown>);
isolatedChannel(window, (m) => {
  if (sink.fn) sink.fn(m);
  else if (sink.q.length < MAX_QUEUED) sink.q.push(m);
});
