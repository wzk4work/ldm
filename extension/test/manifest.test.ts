import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

// run from extension/ (npm test)
const read = (n: string) => JSON.parse(readFileSync(join(process.cwd(), "static", n), "utf8"));

test("Firefox manifest: event-page background, fixed gecko id, no data collection, MAIN-world capable minimum", () => {
  const m = read("manifest.firefox.json");
  assert.equal(m.manifest_version, 3);
  // Firefox MV3 has no extension service worker: background.scripts (event page)
  assert.deepEqual(m.background, { scripts: ["background.js"] });
  const g = m.browser_specific_settings.gecko;
  // the native-messaging manifest's allowed_extensions names this id
  assert.equal(g.id, "ldm@wzk4work.github.io");
  // scripting.registerContentScripts({world: "MAIN"}) (page hook) needs >= 128, built-in data consent >= 140
  assert.ok(parseFloat(g.strict_min_version) >= 140, g.strict_min_version);
  // LDM only talks to the local daemon: nothing is transmitted to us or third parties
  assert.deepEqual(g.data_collection_permissions, { required: ["none"] });
  for (const p of ["nativeMessaging", "webRequest", "downloads", "scripting", "contextMenus"]) assert.ok(m.permissions.includes(p), p);
});

test("Firefox and Chromium manifests ship the same scripts and permissions", () => {
  const f = read("manifest.firefox.json");
  const c = read("manifest.chromium.json");
  assert.deepEqual([...f.permissions].sort(), [...c.permissions].sort());
  assert.deepEqual(f.content_scripts, c.content_scripts);
  assert.equal(f.version, c.version);
});
