"""Proxy auto-config (engine.proxy = "pac") against local proxies.

Two self-written proxies run in-process: an HTTP forward proxy (absolute-URI GET) and a SOCKS5
proxy (CONNECT, IPv4/domain). Each counts what it relayed, so the tests can tell which path a
download really took. A "dead" proxy is a closed local port.
"""

import hashlib
import os
import socket
import threading
import time

import pytest

from conftest import Env  # noqa: F401


def sha256(p) -> str:
    return hashlib.sha256(open(p, "rb").read()).hexdigest()


def _pipe(a: socket.socket, b: socket.socket):
    def one(src, dst):
        try:
            while True:
                d = src.recv(65536)
                if not d:
                    break
                dst.sendall(d)
        except OSError:
            pass
        finally:
            for s in (dst,):
                try:
                    s.shutdown(socket.SHUT_WR)
                except OSError:
                    pass

    t = threading.Thread(target=one, args=(b, a), daemon=True)
    t.start()
    one(a, b)
    t.join(30)
    a.close()
    b.close()


class _Proxy:
    def __init__(self):
        self.ls = socket.socket()
        self.ls.bind(("127.0.0.1", 0))
        self.ls.listen(64)
        self.port = self.ls.getsockname()[1]
        self.seen: list[str] = []
        self.lock = threading.Lock()
        threading.Thread(target=self._accept, daemon=True).start()

    def _accept(self):
        while True:
            try:
                c, _ = self.ls.accept()
            except OSError:
                return
            threading.Thread(target=self._safe, args=(c,), daemon=True).start()

    def _safe(self, c):
        try:
            self.handle(c)
        except Exception:  # noqa: BLE001 - a broken test client must not kill the proxy
            c.close()

    def note(self, what: str):
        with self.lock:
            self.seen.append(what)

    def close(self):
        self.ls.close()


class HttpProxy(_Proxy):
    def handle(self, c: socket.socket):
        buf = b""
        while b"\r\n\r\n" not in buf:
            d = c.recv(65536)
            if not d:
                return c.close()
            buf += d
        head, rest = buf.split(b"\r\n\r\n", 1)
        lines = head.decode("latin-1").split("\r\n")
        method, target, ver = lines[0].split(" ")
        assert target.startswith("http://"), target
        hostport, _, path = target[len("http://") :].partition("/")
        host, _, port = hostport.partition(":")
        self.note(f"{method} /{path}")
        up = socket.create_connection((host, int(port or 80)))
        hdrs = [h for h in lines[1:] if h.split(":")[0].lower() not in ("connection", "proxy-connection", "keep-alive")]
        req = "\r\n".join([f"{method} /{path} {ver}", *hdrs, "Connection: close", "", ""]).encode("latin-1")
        up.sendall(req + rest)
        _pipe(c, up)


class Socks5Proxy(_Proxy):
    def handle(self, c: socket.socket):
        def recv(n):
            d = b""
            while len(d) < n:
                x = c.recv(n - len(d))
                if not x:
                    raise EOFError
                d += x
            return d

        ver, n = recv(2)
        assert ver == 5
        recv(n)
        c.sendall(b"\x05\x00")
        ver, cmd, _, atyp = recv(4)
        assert cmd == 1, cmd
        if atyp == 1:
            host = socket.inet_ntoa(recv(4))
        elif atyp == 3:
            host = recv(recv(1)[0]).decode()
        else:
            host = socket.inet_ntop(socket.AF_INET6, recv(16))
        port = int.from_bytes(recv(2), "big")
        self.note(f"CONNECT {host}:{port}")
        up = socket.create_connection((host, port))
        c.sendall(b"\x05\x00\x00\x01" + socket.inet_aton("0.0.0.0") + b"\x00\x00")
        _pipe(c, up)


def dead_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


@pytest.fixture
def proxies():
    h, s = HttpProxy(), Socks5Proxy()
    yield h, s
    h.close()
    s.close()


def put(env, rel: str, size: int = 300_000) -> str:
    p = env.www / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(os.urandom(size))
    return rel


def fetch(ipc, env, rel: str) -> dict:
    jid = ipc.call("DownloadAdd", {"url": f"{env.base}/{rel}"})["payload"]["jobId"]
    j = ipc.wait_state(jid, "Completed", 60)
    assert sha256(j["final_path"]) == sha256(env.www / rel)
    return j


def log(env) -> str:
    return (env.tmp / "ldmd.log").read_text()


def test_pac_url_routes_per_request_with_fallback_order(env, proxies):
    http, socks = proxies
    dead = dead_port()
    env.start_server()
    for rel in ("via-http/a.bin", "via-socks/b.bin", "dead/c.bin", "plain/d.bin"):
        put(env, rel)
    (env.www / "proxy.pac").write_text(
        f"""// served over HTTP by the test server (fetched without a proxy)
function FindProxyForURL(url, host) {{
    if (shExpMatch(url, "*/via-http/*")) return "PROXY 127.0.0.1:{http.port}; DIRECT";
    if (shExpMatch(url, "*/via-socks/*"))
        return "PROXY 127.0.0.1:{dead}; SOCKS5 127.0.0.1:{socks.port}; DIRECT";
    if (shExpMatch(url, "*/dead/*")) return "PROXY 127.0.0.1:{dead}; SOCKS5 127.0.0.1:{dead}; DIRECT";
    if (isPlainHostName(host) || dnsDomainIs(host, ".invalid")) return "PROXY 127.0.0.1:{dead}";
    return "DIRECT";
}}
"""
    )
    env.write_config({"engine": {"proxy": "pac", "pac_url": env.base + "/proxy.pac"}})
    ipc = env.start_daemon(ws=False)

    fetch(ipc, env, "via-http/a.bin")
    assert http.seen and all(s.endswith("/via-http/a.bin") for s in http.seen), http.seen
    assert socks.seen == []

    fetch(ipc, env, "via-socks/b.bin")  # dead PROXY skipped -> SOCKS5
    port = env.base.rsplit(":", 1)[1]
    assert socks.seen and all(s == f"CONNECT 127.0.0.1:{port}" for s in socks.seen), socks.seen
    assert not any("b.bin" in s for s in http.seen)

    n_http, n_socks = len(http.seen), len(socks.seen)
    fetch(ipc, env, "dead/c.bin")  # both proxies dead -> DIRECT
    fetch(ipc, env, "plain/d.bin")  # DIRECT
    assert (len(http.seen), len(socks.seen)) == (n_http, n_socks)
    assert "PAC script loaded" in log(env)
    assert "falling back to a later proxy" in log(env) and "falling back to DIRECT" in log(env)


def test_pac_file_interval_refresh_and_bad_update_keeps_last_script(env, proxies):
    http, _ = proxies
    env.start_server()
    pac = env.tmp / "local.pac"
    pac.write_text('function FindProxyForURL(url, host) { return "DIRECT"; }\n')
    env.write_config({"engine": {"proxy": "pac", "pac_url": str(pac), "pac_refresh_secs": 1}})
    ipc = env.start_daemon(ws=False)

    fetch(ipc, env, put(env, "one.bin"))
    assert http.seen == []

    pac.write_text(f'function FindProxyForURL(url, host) {{ return "PROXY 127.0.0.1:{http.port}"; }}\n')
    t0 = time.time()
    while log(env).count("PAC script loaded") < 2:
        assert time.time() - t0 < 10, log(env)
        time.sleep(0.1)
    fetch(ipc, env, put(env, "two.bin"))
    assert any(s.endswith("/two.bin") for s in http.seen), http.seen

    pac.write_text("function FindProxyForURL(url, host) { return /regex/; }\n")  # unsupported -> load fails
    t0 = time.time()
    while "PAC load failed" not in log(env):
        assert time.time() - t0 < 10, log(env)
        time.sleep(0.1)
    fetch(ipc, env, put(env, "three.bin"))
    assert any(s.endswith("/three.bin") for s in http.seen), "previous script stays in use"
    assert "keeping_previous=true" in log(env)


def test_pac_unavailable_goes_direct_and_bad_settings_are_rejected(env):
    env.start_server()
    env.write_config({"engine": {"proxy": "pac", "pac_url": env.base + "/missing.pac", "connect_timeout_secs": 2}})
    ipc = env.start_daemon(ws=False)
    fetch(ipc, env, put(env, "x.bin"))  # PAC 404 -> DIRECT with a warning
    assert "PAC load failed" in log(env) and "connecting DIRECT" in log(env)

    r = ipc.call("ConfigSet", {"patch": {"engine": {"pac_url": "relative/proxy.pac"}}})
    assert r["type"] == "Error", r
    r = ipc.call("ConfigGet", {})
    assert r["payload"]["config"]["engine"]["pac_url"] == env.base + "/missing.pac", "rejected setting not saved"
    assert "relative/proxy.pac" not in (env.home / ".config/ldm/config.toml").read_text()


def test_cross_host_redirect_is_routed_by_the_pac_for_the_new_host(env, proxies):
    """reqwest follows redirects itself; the target host gets its own PAC decision."""
    import http.server

    http_proxy, _ = proxies
    env.start_server()
    port = env.base.rsplit(":", 1)[1]
    put(env, "via-redirect/r.bin")

    class Redir(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            self.send_response(302)
            self.send_header("Location", f"http://localhost:{port}/via-redirect/r.bin")
            self.send_header("Content-Length", "0")
            self.end_headers()

        def log_message(self, *a):
            pass

    rs = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Redir)
    threading.Thread(target=rs.serve_forever, daemon=True).start()
    pac = env.tmp / "r.pac"
    pac.write_text(f'function FindProxyForURL(url, host) {{ return host == "localhost" ? "PROXY 127.0.0.1:{http_proxy.port}" : "DIRECT"; }}\n')
    env.write_config({"engine": {"proxy": "pac", "pac_url": f"file://{pac}"}})
    ipc = env.start_daemon(ws=False)
    try:
        jid = ipc.call("DownloadAdd", {"url": f"http://127.0.0.1:{rs.server_address[1]}/go/r.bin"})["payload"]["jobId"]
        j = ipc.wait_state(jid, "Completed", 60)
        assert sha256(j["final_path"]) == sha256(env.www / "via-redirect/r.bin")
        assert http_proxy.seen and all(s.endswith("/via-redirect/r.bin") for s in http_proxy.seen), http_proxy.seen
    finally:
        rs.shutdown()
