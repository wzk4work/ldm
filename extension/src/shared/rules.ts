// Rule-set compilation: host globs, URL patterns, size floor.
import type { RuleSet } from "./protocol";
import { classify, urlExt } from "./classify";

/** Compile a glob (`*` = any run, `?` = one char, `**` = any incl. '/'). */
export function globToRegExp(glob: string, starCrossesSlash = false): RegExp {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        re += ".*";
        i++;
      } else re += starCrossesSlash ? ".*" : "[^/]*";
    } else if (c === "?") re += ".";
    else re += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
  }
  return new RegExp(`^${re}$`, "i");
}

export class CompiledRules {
  readonly hosts: RegExp[];
  readonly patterns: RegExp[];
  constructor(readonly rules: RuleSet) {
    // host globs: "*.example.com" also matches "example.com"
    this.hosts = rules.excludeHosts.flatMap((h) => (h.startsWith("*.") ? [globToRegExp(h), globToRegExp(h.slice(2))] : [globToRegExp(h)]));
    this.patterns = rules.excludeUrlPatterns.map((p) => (p.includes("*") || p.includes("?") ? globToRegExp(p, true) : new RegExp(p.replace(/[.+^${}()|[\]\\*?]/g, "\\$&"), "i")));
  }

  excluded(url: string): boolean {
    let host = "";
    try {
      host = new URL(url).hostname;
    } catch {
      return true;
    }
    if (this.hosts.some((r) => r.test(host))) return true;
    return this.patterns.some((r) => r.test(url));
  }

  /** Should a browser download be handed to LDM? */
  shouldCapture(url: string, mime?: string | null, size?: number | null, filename?: string | null): boolean {
    if (!this.rules.captureEnabled) return false;
    if (!/^https?:/i.test(url)) return false; // blob:, data:, file: stay in the browser
    if (this.excluded(url)) return false;
    if (size != null && size >= 0 && size < this.rules.minSize) return false;
    const ext = filename ? urlExt("x:/" + filename) : urlExt(url);
    if (this.rules.captureExts.includes(ext)) return true;
    const v = classify(url, mime, null, this.rules);
    return v === "file" || v === "media" || v === "manifest";
  }
}
