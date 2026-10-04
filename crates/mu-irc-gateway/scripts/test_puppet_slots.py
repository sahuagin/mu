#!/usr/bin/env python3
"""Offline tests for puppet-slots.py: what it makes of a server's answers,
over a scripted socket. No server, no network; `openssl` for the cert cases.

    cd crates/mu-irc-gateway/scripts && python3 -m unittest -v test_puppet_slots
"""

import importlib.util
import os
import shutil
import socket
import subprocess
import tempfile
import types
import unittest
from contextlib import redirect_stdout
from io import StringIO

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("puppet_slots", os.path.join(HERE, "puppet-slots.py"))
ps = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ps)

S = ":irc.test"
CAPS = "account-tag extended-join account-notify sasl"


class FakeSock:
    """Answers each line the client sends with the lines scripted for its
    prefix (first match wins); silence otherwise, as a timeout."""

    def __init__(self, script):
        self.script = script
        self.sent = []
        self.inbox = b""

    def sendall(self, data):
        line = data.decode().rstrip("\r\n")
        self.sent.append(line)
        for prefix, lines in self.script:
            if line.startswith(prefix):
                self.inbox += "".join(l + "\r\n" for l in lines).encode()
                break

    def recv(self, n):
        if not self.inbox:
            raise socket.timeout()
        out, self.inbox = self.inbox[:n], self.inbox[n:]
        return out

    def settimeout(self, t):
        pass

    def close(self):
        pass


def external(account="cc-1", with_903=True, ack=CAPS, welcome=True, whox=True):
    """A server letting cc-1 in over SASL EXTERNAL, with the knobs the tests turn."""
    auth = [f"{S} 900 cc-1 cc-1!u@h {account} :You are now logged in as {account}"]
    auth.append(f"{S} 903 cc-1 :SASL authentication successful" if with_903 else f"{S} 904 cc-1 :SASL authentication failed")
    end = [f"{S} 001 cc-1 :Welcome", f"{S} 005 cc-1 {'WHOX ' if whox else ''}CASEMAPPING=ascii :are supported", f"{S} 376 cc-1 :End of MOTD"] if welcome else [f"{S} 433 * cc-1 :Nickname is already in use"]
    return [
        ("CAP LS", [f"{S} CAP * LS :{CAPS}"]),
        ("CAP REQ", [f"{S} CAP * ACK :{ack}"] if ack else [f"{S} CAP * NAK :{CAPS}"]),
        ("AUTHENTICATE EXTERNAL", ["AUTHENTICATE +"]),
        ("AUTHENTICATE +", auth),
        ("CAP END", end),
    ]


def login(script, slot="cc-1", want=ps.REQUIRED_CAPS):
    irc = ps.Irc(FakeSock(script))
    return ps.register_connection(irc, slot, want_caps=want, sasl="external"), irc


class Registration(unittest.TestCase):
    def test_external_login_names_the_account(self):
        reg, irc = login(external())
        self.assertEqual(reg.account, "cc-1")
        self.assertTrue(reg.welcomed)
        self.assertIn("WHOX", reg.isupport)
        self.assertEqual(reg.acked, set(CAPS.split()))
        self.assertTrue(ps.logged_in_as(reg, "cc-1"))
        self.assertIn("CAP END", irc.sock.sent)

    def test_900_without_903_is_not_a_login(self):
        reg, _ = login(external(with_903=False))
        self.assertIsNone(reg.account)
        self.assertIn("SASL failed", reg.error)
        self.assertFalse(ps.logged_in_as(reg, "cc-1"))

    def test_the_wrong_account_is_not_the_slot(self):
        reg, _ = login(external(account="cc-2"))
        self.assertEqual(reg.account, "cc-2")
        self.assertFalse(ps.logged_in_as(reg, "cc-1"))
        self.assertTrue(ps.logged_in_as(reg, "CC-2"))  # account names casefold

    def test_sasl_not_acknowledged_is_not_a_login(self):
        reg, irc = login(external(ack=""))
        self.assertIsNone(reg.account)
        self.assertIn("acknowledge", reg.error)
        self.assertFalse(any(l.startswith("AUTHENTICATE") for l in irc.sock.sent))

    def test_a_refused_nick_is_not_welcomed(self):
        reg, _ = login(external(welcome=False))
        self.assertEqual(reg.account, "cc-1")
        self.assertFalse(reg.welcomed)
        self.assertIn("433", reg.error)
        self.assertFalse(ps.logged_in_as(reg, "cc-1"))

    def test_a_nick_refused_at_once_is_reported_as_such(self):
        # The server answers NICK the moment it is sent, before CAP END.
        script = external()
        script.insert(0, ("NICK", [f"{S} 433 * cc-1 :Nickname is already in use"]))
        reg, irc = login(script)
        self.assertFalse(reg.welcomed)
        self.assertIn("433", reg.error)
        sent = irc.sock.sent
        self.assertLess(sent.index("AUTHENTICATE +"), sent.index("NICK cc-1"))
        self.assertLess(sent.index("NICK cc-1"), sent.index("CAP END"))

    def test_only_acknowledged_caps_count(self):
        reg, _ = login(external(ack="sasl", whox=False))
        self.assertEqual(reg.acked, {"sasl"})
        self.assertNotIn("WHOX", reg.isupport)

    def test_silence_is_an_error_not_a_login(self):
        reg, _ = login([("CAP LS", [f"{S} CAP * LS :{CAPS}"])])
        self.assertIsNone(reg.account)
        self.assertFalse(reg.welcomed)
        self.assertIsNotNone(reg.error)


def ns(*lines):
    return [f":NickServ!NickServ@localhost NOTICE cc-1 :{l}" for l in lines]


class AlwaysOn(unittest.TestCase):
    def state(self, *lines):
        irc = ps.Irc(FakeSock([("NS GET always-on", ns(*lines))]))
        return ps.always_on_state(irc)

    def test_off(self):
        state, reply = self.state("Your stored always-on setting is: default", "Given current server settings, your client is not always-on")
        self.assertEqual(state, "off")
        self.assertIn(" / ", reply)

    def test_on(self):
        self.assertEqual(self.state("Your stored always-on setting is: enabled", "Given current server settings, your client is always-on")[0], "on")

    def test_unknown_wording_is_not_off(self):
        state, reply = self.state("Setting not found")
        self.assertIsNone(state)
        self.assertEqual(reply, "Setting not found")

    def test_silence_is_not_off(self):
        self.assertEqual(self.state(), (None, None))


class Arguments(unittest.TestCase):
    def test_a_pool_has_at_least_one_slot(self):
        for count in ("0", "-2"):
            with self.assertRaises(SystemExit) as e, redirect_stdout(StringIO()):
                import contextlib, io
                with contextlib.redirect_stderr(io.StringIO()):
                    ps.main(["config", "--dir", "/nonexistent", "--count", count])
            self.assertEqual(e.exception.code, 2)


class Parsing(unittest.TestCase):
    def test_endpoints(self):
        self.assertEqual(ps.parse_endpoint("irc.example.org:6697"), ("irc.example.org", 6697))
        self.assertEqual(ps.parse_endpoint("127.0.0.1:16697"), ("127.0.0.1", 16697))
        self.assertEqual(ps.parse_endpoint("[::1]:6697"), ("::1", 6697))
        self.assertEqual(ps.parse_endpoint("[fe80::1%em0]:6697"), ("fe80::1%em0", 6697))
        for bad in ("irc.example.org", "::1", "::1:6697", "[::1]", "host:port", ":6697"):
            with self.assertRaises(ValueError, msg=bad):
                ps.parse_endpoint(bad)

    def test_whox_rows(self):
        rows = ps.whox_rows([
            f"{S} 354 me 7 #mu u h irc.test cc-1 H cc-1",
            f"{S} 354 me 7 #mu u h irc.test probe H :0",
            f"{S} 354 me 7 #mu short",
            f"{S} 315 me #mu :End of WHO list",
        ])
        self.assertEqual(rows, [("cc-1", "cc-1"), ("probe", "0")])

    def test_nickserv_reply_is_joined(self):
        irc = ps.Irc(FakeSock([("NS REGISTER", ns("Account created", "You're now logged in as cc-1"))]))
        irc.send("NS REGISTER x")
        self.assertEqual(irc.nickserv(), "Account created / You're now logged in as cc-1")
        irc.send("NS INFO")
        self.assertIsNone(irc.nickserv())


def probe(join=None, who=None):
    """A server letting an unauthenticated probe in and into #mu; `who` is
    the answer to the WHO (None: silence), `join` overrides the JOIN answer."""
    return [
        ("CAP LS", [f"{S} CAP * LS :{CAPS}"]),
        ("CAP REQ", [f"{S} CAP * ACK :extended-join account-tag"]),
        ("CAP END", [f"{S} 001 probe :Welcome", f"{S} 005 probe WHOX :are supported", f"{S} 376 probe :End of MOTD"]),
        ("JOIN", join if join is not None else [":probe!u@h JOIN #mu", f"{S} 353 probe = #mu :probe cc-1", f"{S} 366 probe #mu :End of NAMES"]),
        ("WHO", who if who is not None else []),
    ]


WHOX = [f"{S} 354 probe 7 #mu u h irc.test probe H 0", f"{S} 354 probe 7 #mu u h irc.test cc-1 H cc-1", f"{S} 315 probe #mu :End of WHO list"]


class Smoke(unittest.TestCase):
    def run_smoke(self, script):
        ps.CONNECT = lambda *a, **k: ps.Irc(FakeSock(script))
        self.addCleanup(setattr, ps, "CONNECT", ps.Irc.connect)
        args = types.SimpleNamespace(server="irc.test:6697", ca=None, no_hostname_check=False, lobby="#mu", nick="probe")
        out = StringIO()
        with redirect_stdout(out):
            try:
                rc = ps.cmd_smoke(args)
            except SystemExit as e:
                return e.code, out.getvalue()
        return rc, out.getvalue()

    def test_shows_members_and_accounts(self):
        rc, out = self.run_smoke(probe(who=WHOX))
        self.assertEqual(rc, 0)
        self.assertIn("NAMES #mu: probe cc-1", out)
        self.assertIn("account=cc-1", out)

    def test_silence_after_who_is_a_failure(self):
        rc, _ = self.run_smoke(probe(who=[]))
        self.assertIn("no end of the WHO list", rc)

    def test_ordinary_who_replies_are_not_whox(self):
        rc, _ = self.run_smoke(probe(who=[f"{S} 352 probe #mu u h irc.test probe H :0 real", f"{S} 315 probe #mu :End of WHO list"]))
        self.assertIn("no WHOX row for probe", rc)

    def test_refused_join_is_a_failure(self):
        rc, _ = self.run_smoke(probe(join=[f"{S} 473 probe #mu :Cannot join channel (+i)"]))
        self.assertIn("refused", rc)
        self.assertIn("+i", rc)


@unittest.skipUnless(shutil.which("openssl"), "openssl not on PATH")
class Certs(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)

    def certs(self, count=1, prefix="t"):
        args = types.SimpleNamespace(dir=self.tmp, prefix=prefix, count=count, days=1)
        out = StringIO()
        with redirect_stdout(out):
            rc = ps.cmd_certs(args)
        return rc, out.getvalue(), args

    def test_mint_then_keep(self):
        rc, out, _ = self.certs(2)
        self.assertEqual(rc, 0)
        self.assertEqual(out.count("minted"), 2)
        fp = ps.fingerprint(os.path.join(self.tmp, "t-1.crt"))
        rc, out, _ = self.certs(2)
        self.assertEqual(rc, 0)
        self.assertEqual(out.count("exists, kept"), 2)
        self.assertEqual(ps.fingerprint(os.path.join(self.tmp, "t-1.crt")), fp)
        self.assertEqual(oct(os.stat(os.path.join(self.tmp, "t-1.key")).st_mode & 0o777), "0o600")

    def test_partial_pair_is_refused_not_overwritten(self):
        self.certs(1)
        crt = os.path.join(self.tmp, "t-1.crt")
        os.remove(os.path.join(self.tmp, "t-1.key"))
        fp = ps.fingerprint(crt)
        rc, out, args = self.certs(1)
        self.assertEqual(rc, 1)
        self.assertIn("only", out)
        self.assertEqual(ps.fingerprint(crt), fp)
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "t-1.key")))
        out = StringIO()
        with redirect_stdout(out):
            self.assertIsNone(ps.pair_ready(args, "t-1", set()))
        self.assertIn("partial", out.getvalue())

    def test_lone_key_is_refused_without_a_traceback(self):
        self.certs(1)
        os.remove(os.path.join(self.tmp, "t-1.crt"))
        rc, out, _ = self.certs(1)
        self.assertEqual(rc, 1)
        self.assertIn("t-1.key exists", out)

    def test_unreadable_certificate_is_refused_without_a_traceback(self):
        _, _, args = self.certs(1)
        with open(os.path.join(self.tmp, "t-1.crt"), "w") as f:
            f.write("not a certificate\n")
        rc, out, _ = self.certs(1)
        self.assertEqual(rc, 1)
        self.assertIn("cannot read", out)
        out = StringIO()
        with redirect_stdout(out):
            self.assertIsNone(ps.pair_ready(args, "t-1", set()))
        self.assertIn("cannot read", out.getvalue())

    def test_mismatched_pair_is_refused(self):
        self.certs(2)
        shutil.copy(os.path.join(self.tmp, "t-2.key"), os.path.join(self.tmp, "t-1.key"))
        rc, out, args = self.certs(2)
        self.assertEqual(rc, 1)
        self.assertIn("do not match", out)
        out = StringIO()
        with redirect_stdout(out):
            self.assertIsNone(ps.pair_ready(args, "t-1", set()))
            self.assertIsNotNone(ps.pair_ready(args, "t-2", set()))
        self.assertIn("do not match", out.getvalue())

    def test_a_pair_that_appears_while_minting_is_not_overwritten(self):
        other = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, other)
        args = types.SimpleNamespace(dir=other, prefix="t", count=1, days=1)
        with redirect_stdout(StringIO()):
            ps.cmd_certs(args)
        theirs = ps.fingerprint(os.path.join(other, "t-1.crt"))

        def race():  # another run publishes its pair first
            for name in ("t-1.crt", "t-1.key"):
                shutil.copy(os.path.join(other, name), os.path.join(self.tmp, name))

        ps.BEFORE_PUBLISH = race
        self.addCleanup(setattr, ps, "BEFORE_PUBLISH", lambda: None)
        rc, out, _ = self.certs(1)
        self.assertEqual(rc, 1)
        self.assertIn("appeared", out)
        self.assertEqual(ps.fingerprint(os.path.join(self.tmp, "t-1.crt")), theirs)
        self.assertEqual(sorted(os.listdir(self.tmp)), ["t-1.crt", "t-1.key"])

    def test_fixture_certificates_are_refused(self):
        fixture = os.path.join(ps.FIXTURES, "slot-a.pem")
        if not os.path.exists(fixture):
            self.skipTest("fixtures not present")
        shutil.copy(fixture, os.path.join(self.tmp, "t-1.crt"))
        shutil.copy(os.path.join(ps.FIXTURES, "slot-a.key.pem"), os.path.join(self.tmp, "t-1.key"))
        args = types.SimpleNamespace(dir=self.tmp, prefix="t", count=1)
        out = StringIO()
        with redirect_stdout(out):
            self.assertIsNone(ps.pair_ready(args, "t-1", ps.fixture_fingerprints()))
        self.assertIn("test fixture", out.getvalue())


if __name__ == "__main__":
    unittest.main()
