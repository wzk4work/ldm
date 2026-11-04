#!/usr/bin/env python3
"""Generate cargo-sources.json (flatpak-builder sources) from Cargo.lock.

Every crates.io package becomes an `archive` source unpacked into cargo/vendor/<name>-<version>
with a `.cargo-checksum.json`, plus a cargo config that replaces crates.io with that directory,
so `CARGO_HOME=$PWD/cargo cargo build --offline` works without network in the sandbox.

Usage: gen-cargo-sources.py [Cargo.lock] [out.json]   (defaults: repo Cargo.lock, ./cargo-sources.json)
"""

import json
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def sources(lock: dict) -> list[dict]:
    out: list[dict] = []
    for pkg in sorted(lock.get("package", []), key=lambda p: (p["name"], p["version"])):
        src = pkg.get("source")
        if src is None:  # workspace member
            continue
        if src != REGISTRY:
            raise SystemExit(f"unsupported source for {pkg['name']}: {src}")
        name, ver, sha = pkg["name"], pkg["version"], pkg["checksum"]
        dest = f"cargo/vendor/{name}-{ver}"
        out.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"https://static.crates.io/crates/{name}/{name}-{ver}.crate",
                "sha256": sha,
                "dest": dest,
            }
        )
        out.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": sha, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )
    out.append(
        {
            "type": "inline",
            "contents": '[source.vendored-sources]\ndirectory = "cargo/vendor"\n\n'
            '[source.crates-io]\nreplace-with = "vendored-sources"\n',
            "dest": "cargo",
            "dest-filename": "config.toml",
        }
    )
    return out


def main() -> None:
    lock_path = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE.parent.parent / "Cargo.lock"
    out_path = Path(sys.argv[2]) if len(sys.argv) > 2 else HERE / "cargo-sources.json"
    data = sources(tomllib.loads(lock_path.read_text()))
    out_path.write_text(json.dumps(data, indent=1) + "\n")
    print(f"{out_path}: {sum(1 for s in data if s['type'] == 'archive')} crates")


if __name__ == "__main__":
    main()
