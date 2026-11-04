# Security policy

## Reporting a vulnerability

Please do **not** open a public issue for security problems. Use GitHub's private
vulnerability reporting instead: the "Security" tab of the repository, then "Report a
vulnerability". Include the version, how to reproduce the problem, and its impact. You should
get a first reply within 7 days.

Only the latest release is supported.

## Local attack surface

LDM runs as the desktop user and does not need root. These are the interfaces it opens and how
they are protected.

### Daemon socket (`ldmd`)

- Unix socket at `$XDG_RUNTIME_DIR/ldm.sock` (or `LDM_SOCKET`), mode `0600`.
- Peers are checked with `SO_PEERCRED`: only the same UID may connect.
- Anything that runs as your user can add downloads through it. That is by design: it is the
  same trust boundary as your home directory.

### Native messaging host (`ldm-native-host`)

- It is started by the browser over stdio and relays JSON envelopes to the daemon. It opens no
  network port.
- The manifests (`io.github.wzk4work.ldm.browser_host.json`) allow only:
  - in Chromium browsers, the extension ID in `extension/EXTENSION_ID` (`allowed_origins`);
  - in Firefox, the add-on ID `ldm@wzk4work.github.io` (`allowed_extensions`).
- Messages larger than 1 MiB are rejected.
- If you install a build of the extension with a different ID (for example from a store), the
  manifests have to list that ID.

### WebSocket bridge (port 17001, off by default)

- The bridge is optional: `ldmd --ws` or `browser.ws_bridge_enabled = true`. The extension
  normally uses native messaging and does not need it.
- It listens on loopback only (`127.0.0.1`, port 17001, or the next free port of up to 10
  following ports). A non-loopback `ws_bridge_host` is refused.
- Origin check: requests that carry an `http://` or `https://` Origin (that is, web pages) are
  rejected. Allowed are `chrome-extension://`, `moz-extension://`, or no Origin header.
- Token: when `browser.ws_bridge_token` is set, every client must send it in the
  `X-LDM-Token` header or as `?token=`. **Without a token, any local process can use the
  bridge.** Set a long random token if you enable it on a multi-user machine.
- Subprotocol `ldm.bridge.v1` (JSON envelopes); messages larger than 1 MiB are rejected.

### Browser extension

- It needs broad host permissions to see media requests. It sends URLs, page titles and, for a
  download the user starts, the cookies of that site to the local daemon only. Nothing is sent
  to any other server.
- The optional page hook (off by default) only observes `fetch`/XHR URLs and Media Source
  Extensions counters in the page. It never reads or changes media data, requests or responses.

## Downloaded helper tools

yt-dlp and deno are not shipped. They are downloaded only after the user agrees, over HTTPS
from the official GitHub releases, and verified before use:

- yt-dlp: SHA-256 from `SHA2-256SUMS`, plus the release's GPG signature when `gpg` is installed.
  The signing key is embedded and its fingerprint is pinned.
- deno: SHA-256 from the release's checksum file or from a checksum pinned in LDM.

## Out of scope

LDM does not bypass DRM, logins, paywalls, CAPTCHAs or other anti-bot measures. Reports that ask
for such features will be closed.
