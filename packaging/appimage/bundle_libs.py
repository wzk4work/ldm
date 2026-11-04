#!/usr/bin/env python3
"""Copy the shared-library closure of the ELF files under an AppDir.

usage: bundle_libs.py APPDIR SITE_SRC SITE_DST SYS_DST
Walks DT_NEEDED (readelf) from every ELF already in APPDIR, resolving names with ldd (falling back
to the source wheel's library dirs, since the AppDir is still incomplete). A library that lives in
the source site-packages (PySide6/Qt/lib, shiboken6) goes to the same relative path under
SITE_DST, where the wheel's own $ORIGIN runpaths find it; other host libraries go to SYS_DST,
except the ones that must come from the host (glibc, GPU/display stack, fontconfig, D-Bus, glib…)."""

import os
import re
import shutil
import subprocess
import sys

HOST_ONLY = re.compile(
    r"^(ld-linux.*|linux-vdso.*|libc|libm|libdl|libpthread|librt|libresolv|libutil|libanl|libnsl|libmvec"
    r"|libgcc_s|libstdc\+\+"
    r"|libGL|libGLX|libEGL|libGLdispatch|libOpenGL|libGLESv2|libdrm|libgbm|libvulkan"
    r"|libX11|libX11-xcb|libXau|libXdmcp|libXext|libXrender|libXi|libXfixes|libSM|libICE"
    r"|libxcb|libxcb-(dri2|dri3|glx|present|sync|xfixes|randr|render|shape|shm)"
    r"|libwayland-(client|cursor|egl|server)"
    r"|libfontconfig|libfreetype|libexpat|libz|libdbus-1|libsystemd|libudev|libasound|libpulse"
    r"|libglib-2\.0|libgobject-2\.0|libgio-2\.0|libgthread-2\.0|libgmodule-2\.0)\.so(\.|$)"
)


def is_elf(p):
    try:
        with open(p, "rb") as f:
            return f.read(4) == b"\x7fELF"
    except OSError:
        return False


def needed(p):
    out = subprocess.run(["readelf", "-d", p], capture_output=True, text=True).stdout
    return re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", out)


def resolved(p, env):
    out = subprocess.run(["ldd", p], capture_output=True, text=True, env=env).stdout
    return {m.group(1): m.group(2) for m in re.finditer(r"^\s*(\S+) => (/\S+)", out, re.M)}


def main():
    appdir, site_src, site_dst, sys_dst = (os.path.realpath(a) for a in sys.argv[1:5])
    os.makedirs(sys_dst, exist_ok=True)
    fallback = [os.path.join(site_src, "PySide6", "Qt", "lib"), os.path.join(site_src, "PySide6"), os.path.join(site_src, "shiboken6"), sys_dst]
    env = dict(os.environ, LD_LIBRARY_PATH=":".join(fallback))
    todo = [os.path.join(d, f) for d, _, fs in os.walk(appdir) for f in fs]
    todo = [p for p in todo if not os.path.islink(p) and is_elf(p)]
    seen, missing, copied = set(), set(), []
    while todo:
        p = todo.pop()
        if p in seen:
            continue
        seen.add(p)
        res = resolved(p, env)
        for name in needed(p):
            if HOST_ONLY.match(name):
                continue
            src = res.get(name)
            if not src:
                missing.add(name)
                continue
            src = os.path.realpath(src)
            if src.startswith(appdir + "/"):
                continue
            if src.startswith(site_src + "/"):
                dst = os.path.join(site_dst, os.path.relpath(os.path.dirname(src), site_src), name)
            else:
                dst = os.path.join(sys_dst, name)
            if not os.path.exists(dst):
                shutil.copy2(src, dst)
                copied.append(os.path.relpath(dst, appdir))
                todo.append(dst)
    for c in sorted(copied):
        print("  + " + c)
    if missing:
        print("unresolved (must exist on the host): " + ", ".join(sorted(missing)))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
