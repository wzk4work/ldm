"""FTPS (explicit AUTH TLS / implicit port 990 style) against pyftpdlib's TLS_FTPHandler.

The daemon trusts the throwaway test CA only through LDM_TEST_FTPS_CA, which debug builds honour
(release builds ignore it). Certificate verification is otherwise always on.
"""

import datetime
import hashlib
import ipaddress
import os
import threading

import pytest

from conftest import Env

pytest.importorskip("pyftpdlib")
OpenSSL = pytest.importorskip("OpenSSL")
from cryptography import x509  # noqa: E402
from cryptography.hazmat.primitives import hashes, serialization  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import ec  # noqa: E402
from cryptography.x509.oid import NameOID  # noqa: E402
from OpenSSL import SSL  # noqa: E402
from OpenSSL._util import lib as _ssl_lib  # noqa: E402
from pyftpdlib.authorizers import DummyAuthorizer  # noqa: E402
from pyftpdlib.handlers import FTPHandler, TLS_DTPHandler, TLS_FTPHandler  # noqa: E402
from pyftpdlib.ioloop import IOLoop  # noqa: E402
from pyftpdlib.servers import FTPServer  # noqa: E402

DATA = os.urandom(3 * 1024 * 1024 + 4321)
SHA = hashlib.sha256(DATA).hexdigest()


def _name(cn):
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])


def _cert(subject, key, issuer, issuer_key, sans=None, ca=False):
    now = datetime.datetime.now(datetime.UTC)
    b = (
        x509.CertificateBuilder()
        .subject_name(_name(subject))
        .issuer_name(_name(issuer))
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(ca=ca, path_length=None), critical=True)
    )
    if ca:
        b = b.add_extension(x509.KeyUsage(False, False, False, False, False, True, True, False, False), critical=True)
    if sans:
        b = b.add_extension(x509.SubjectAlternativeName(sans), critical=False)
    return b.sign(issuer_key, hashes.SHA256())


def _pem(c):
    return c.public_bytes(serialization.Encoding.PEM)


def _key_pem(k):
    return k.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption())


@pytest.fixture(scope="module")
def pki(tmp_path_factory):
    d = tmp_path_factory.mktemp("pki")
    out = {}
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca = _cert("LDM Test CA", ca_key, "LDM Test CA", ca_key, ca=True)
    (d / "ca.pem").write_bytes(_pem(ca))
    out["ca"] = d / "ca.pem"
    rogue_key = ec.generate_private_key(ec.SECP256R1())
    rogue = _cert("Rogue CA", rogue_key, "Rogue CA", rogue_key, ca=True)
    local = [x509.IPAddress(ipaddress.ip_address("127.0.0.1")), x509.DNSName("localhost")]
    for name, issuer, ikey, sans in (
        ("good", ca, ca_key, local),
        ("untrusted", rogue, rogue_key, local),
        ("wrongname", ca, ca_key, [x509.DNSName("other.invalid")]),
    ):
        k = ec.generate_private_key(ec.SECP256R1())
        c = _cert(name, k, issuer.subject.rfc4514_string()[3:], ikey, sans=sans)
        p = d / f"{name}.pem"
        p.write_bytes(_pem(c) + _key_pem(k))
        out[name] = p
    return out


class Server:
    """pyftpdlib in a background thread; records TLS state of every data connection."""

    def __init__(self, root, cert=None, implicit=False, require_tls=True):
        self.data_conns = []
        auth = DummyAuthorizer()
        auth.add_user("alice", "s3cret", str(root), perm="elr")
        auth.add_anonymous(str(root))
        rec = self.data_conns
        if cert is None:
            handler = type("Plain", (FTPHandler,), {})
        else:
            ctx = SSL.Context(SSL.TLS_SERVER_METHOD)
            ctx.use_certificate_chain_file(str(cert))
            ctx.use_privatekey_file(str(cert))
            ctx.set_options(SSL.OP_NO_SSLv2 | SSL.OP_NO_SSLv3 | SSL.OP_NO_COMPRESSION)
            ctx.set_session_id(b"ldm-ftps-test")
            ctx.set_session_cache_mode(SSL.SESS_CACHE_SERVER)

            class DTP(TLS_DTPHandler):
                def handle_ssl_established(self):
                    rec.append({"reused": bool(_ssl_lib.SSL_session_reused(self.socket._ssl)), "version": self.socket.get_protocol_version_name()})
                    super().handle_ssl_established()

            attrs = {"ssl_context": ctx, "dtp_handler": DTP, "tls_control_required": require_tls, "tls_data_required": require_tls}
            if implicit:

                def handle(self):
                    self.secure_connection(self.ssl_context)
                    TLS_FTPHandler.handle(self)

                attrs["handle"] = handle
            handler = type("TLS", (TLS_FTPHandler,), attrs)
        handler.authorizer = auth
        handler.passive_ports = None
        self.server = FTPServer(("127.0.0.1", 0), handler, ioloop=IOLoop())  # own loop per server
        self.port = self.server.socket.getsockname()[1]
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"timeout": 0.05}, daemon=True)
        self.thread.start()

    def close(self):
        self.server.close_all()


@pytest.fixture
def ftp_env(tmp_path, pki):
    e = Env(tmp_path)
    e.env["LDM_TEST_FTPS_CA"] = str(pki["ca"])
    (e.www / "f.bin").write_bytes(DATA)
    servers = []

    def serve(**kw):
        s = Server(e.www, **kw)
        servers.append(s)
        return s

    e.serve = serve
    yield e
    e.stop()
    for s in servers:
        s.close()


def _sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()


def _add(ipc, url):
    return ipc.call("DownloadAdd", {"url": url})["payload"]["jobId"]


def test_explicit_ftpes_prot_p_with_session_reuse(ftp_env, pki):
    srv = ftp_env.serve(cert=pki["good"])
    ipc = ftp_env.start_daemon(ws=False)
    j = ipc.wait_state(_add(ipc, f"ftpes://alice:s3cret@127.0.0.1:{srv.port}/f.bin"), "Completed", 60)
    assert _sha(j["final_path"]) == SHA
    # data channel was protected (PROT P) and resumed the control channel's TLS session
    assert srv.data_conns, "no TLS data connection seen by the server"
    assert all(c["reused"] for c in srv.data_conns), srv.data_conns
    log = (ftp_env.tmp / "ldmd.log").read_text()
    assert "FTPS data channel (PROT P)" in log
    assert "s3cret" not in log


def test_implicit_ftps(ftp_env, pki):
    srv = ftp_env.serve(cert=pki["good"], implicit=True)
    ipc = ftp_env.start_daemon(ws=False)
    j = ipc.wait_state(_add(ipc, f"ftps://127.0.0.1:{srv.port}/f.bin"), "Completed", 60)
    assert _sha(j["final_path"]) == SHA
    assert srv.data_conns and all(c["reused"] for c in srv.data_conns), srv.data_conns


def test_tls_required_but_server_is_plain(ftp_env):
    srv = ftp_env.serve(cert=None)
    ipc = ftp_env.start_daemon(ws=False)
    j = ipc.wait_state(_add(ipc, f"ftpes://127.0.0.1:{srv.port}/f.bin"), "Failed", 30)
    assert "refused AUTH TLS" in (j.get("error") or ""), j
    assert not (ftp_env.dl / "f.bin").exists()


def test_global_try_mode_upgrades_and_falls_back(ftp_env, pki):
    tls = ftp_env.serve(cert=pki["good"])  # requires TLS: only an upgraded session can log in
    plain = ftp_env.serve(cert=None)
    ftp_env.write_config({"ftp": {"tls": "try"}})
    ipc = ftp_env.start_daemon(ws=False)
    a = ipc.wait_state(_add(ipc, f"ftp://127.0.0.1:{tls.port}/f.bin"), "Completed", 60)
    assert tls.data_conns, "ftp:// with ftp.tls=try did not upgrade"
    b = ipc.wait_state(_add(ipc, f"ftp://127.0.0.1:{plain.port}/f.bin?x"), "Completed", 60)
    assert _sha(a["final_path"]) == SHA and _sha(b["final_path"]) == SHA


def test_certificate_verification_and_per_site_override(ftp_env, pki):
    bad = ftp_env.serve(cert=pki["untrusted"])
    wrong = ftp_env.serve(cert=pki["wrongname"])
    ipc = ftp_env.start_daemon(ws=False)
    for srv in (bad, wrong):
        j = ipc.wait_state(_add(ipc, f"ftpes://127.0.0.1:{srv.port}/f.bin"), "Failed", 30)
        assert "certificate" in (j.get("error") or "").lower(), j
        assert not srv.data_conns
    # opt-in, per host: accept an invalid certificate for 127.0.0.1 only
    r = ipc.call("ConfigSet", {"patch": {"ftp": {"tls": "off"}, "sites": {"tls": [{"host": "127.0.0.1", "accept_invalid_cert": True}]}}})
    assert r["type"] != "Error", r
    for i, srv in enumerate((bad, wrong)):
        j = ipc.wait_state(_add(ipc, f"ftpes://127.0.0.1:{srv.port}/f.bin?{i}"), "Completed", 60)
        assert _sha(j["final_path"]) == SHA
    assert "certificate verification is off" in (ftp_env.tmp / "ldmd.log").read_text()
