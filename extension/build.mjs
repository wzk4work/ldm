// Build script: bundles TypeScript with esbuild into dist/chromium and dist/firefox.
import { build } from "esbuild";
import { cpSync, mkdirSync, rmSync, readdirSync, copyFileSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const tests = process.argv.includes("--tests");
// Store build (`npm run build:store` or LDM_STORE_BUILD=1): drops the yt-dlp / YouTube page rules
// and the pinned "key", and writes to dist-store/ so the default dist/ build is unchanged.
const store = process.argv.includes("--store") || process.env.LDM_STORE_BUILD === "1";

if (tests) {
  rmSync("dist-test", { recursive: true, force: true });
  const entries = readdirSync("test").filter((f) => f.endsWith(".test.ts")).map((f) => join("test", f));
  await build({ entryPoints: entries, outdir: "dist-test", bundle: true, platform: "node", format: "esm", target: "node20", outExtension: { ".js": ".mjs" }, logLevel: "warning", define: { __LDM_STORE_BUILD__: "false" } });
} else {
  for (const target of ["chromium", "firefox"]) {
    const out = join(store ? "dist-store" : "dist", target);
    rmSync(out, { recursive: true, force: true });
    mkdirSync(out, { recursive: true });
    await build({
      entryPoints: { background: "src/background.ts", content: "src/content.ts", popup: "src/popup.ts", links: "src/links.ts", pagehook: "src/pagehook.ts", pagebridge: "src/pagebridge.ts" },
      outdir: out,
      bundle: true,
      format: "iife",
      target: target === "firefox" ? "firefox140" : "chrome110",
      define: { __LDM_STORE_BUILD__: store ? "true" : "false" },
      logLevel: "warning",
    });
    cpSync("static/_locales", join(out, "_locales"), { recursive: true });
    cpSync("static/icons", join(out, "icons"), { recursive: true });
    copyFileSync("static/popup.html", join(out, "popup.html"));
    copyFileSync("static/links.html", join(out, "links.html"));
    if (store) {
      // stores assign their own extension ID and refuse uploads that pin one with "key"
      const manifest = JSON.parse(readFileSync(`static/manifest.${target}.json`, "utf8"));
      delete manifest.key;
      writeFileSync(join(out, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
    } else {
      copyFileSync(`static/manifest.${target}.json`, join(out, "manifest.json"));
    }
    console.log(`built ${out}`);
  }
}
