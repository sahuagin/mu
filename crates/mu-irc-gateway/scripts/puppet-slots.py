#!/usr/bin/env python3
"""Provision an IRC server's puppet slot accounts for mu-irc-gateway, and check them.

A puppet is known by its slot ACCOUNT (`<prefix>-1` .. `<prefix>-N`), and its
credential is that account's client certificate (SASL EXTERNAL / certfp). This
script takes a server from nothing to a provisioned pool, in steps that are
each safe to run again:

    certs     mint a self-signed certificate per slot into the slots directory
    register  create the slot accounts and attach their certificate fingerprints
    verify    log in as every slot over SASL EXTERNAL; check the capabilities
    config    print the `[irc.puppets]` lines for the gateway config
    smoke     join the lobby as a probe and show who is there, and as whom

Standard library only; `openssl` on PATH. Nothing secret is printed or kept:
a registration password is random and discarded once the certificate is
attached, which is the credential from then on. Run it against a throwaway
server first; the same invocations are the deployment against the real one.

`register` has two modes. OPEN (the server allows `NS REGISTER`): each slot
registers itself from its own TLS connection, and Ergo attaches the presented
certificate's fingerprint at registration. OPER (`--oper NAME --oper-pass-file
FILE`, for a server with registration closed): one connection as the operator
creates each account with `NS SAREGISTER` and attaches its fingerprint with
`NS CERT ADD <account> <fingerprint>`; no connection is ever made as a slot
with a password. Both modes register their first connection unauthenticated,
so a server that requires SASL on every connection is not one `register` can
provision; `verify` works there (it logs in as each slot), `smoke` does not
(its probe is unauthenticated).

Every judgement is the server's, not the script's: a slot counts as
provisioned only when its certificate logs in and the server names that slot
as the account (900 then 903, then 001). A reply the script does not
understand is a failure, printed as received, never a pass.

`smoke` joins the lobby as a human-looking probe: a running gateway fronts it
on the mesh for the moment it is there.
"""

import argparse
import base64
import os
import secrets
import socket
import ssl
import subprocess
import sys
import time

STEP = 8.0  # seconds to wait for any one reply
QUIET = 0.8  # a multi-line NickServ reply is over once it pauses this long

SASL_FAIL = ("902", "904", "905", "906", "907", "908")
REG_FAIL = ("432", "433", "436", "437", "464", "465")


# ───────────────────────────── a small IRC client ────────────────────────────


def parse_endpoint(server):
    """`host:port`; an IPv6 literal goes in brackets, `[::1]:6697`."""
    if server.startswith("["):
        host, sep, port = server[1:].partition("]:")
        if not sep:
            raise ValueError(f"{server!r}: expected [v6-address]:port")
    else:
        host, sep, port = server.rpartition(":")
        if not sep or ":" in host:
            raise ValueError(f"{server!r}: expected host:port (brackets around an IPv6 address)")
    if not host or not port.isdigit():
        raise ValueError(f"{server!r}: expected host:port")
    return host, int(port)


class Irc:
    """One connection, read a line at a time. `sock` is anything with
    sendall / recv / settimeout / close; the tests hand in a scripted one."""

    def __init__(self, sock):
        self.sock = sock
        self.buf = b""

    @classmethod
    def connect(cls, server, ca=None, cert=None, key=None, check_hostname=True):
        host, port = parse_endpoint(server)
        ctx = ssl.create_default_context(cafile=ca)
        ctx.check_hostname = check_hostname
        if cert:
            ctx.load_cert_chain(cert, key)
        raw = socket.create_connection((host, port), timeout=STEP)
        try:
            sock = ctx.wrap_socket(raw, server_hostname=host)
        except BaseException:
            raw.close()
            raise
        sock.settimeout(STEP)
        return cls(sock)

    def send(self, line):
        self.sock.sendall((line + "\r\n").encode())

    def wait(self, pred, limit=400):
        """Read lines until `pred(line)` holds, answering PINGs on the way.
        Returns (lines read, the matching line or None on silence)."""
        seen = []
        for _ in range(limit):
            while b"\r\n" not in self.buf:
                try:
                    chunk = self.sock.recv(4096)
                except socket.timeout:
                    return seen, None
                if not chunk:
                    return seen, None
                self.buf += chunk
            raw, self.buf = self.buf.split(b"\r\n", 1)
            line = raw.decode("utf-8", "replace")
            seen.append(line)
            if line.startswith("PING"):
                self.send("PONG" + line[4:])
            if pred(line):
                return seen, line
        return seen, None

    def numeric(self, *codes):
        """The next line carrying one of these numerics, or an ERROR."""
        want = {f" {c} " for c in codes}
        return self.wait(lambda l: l.startswith("ERROR") or any(w in l for w in want))

    def nickserv(self):
        """NickServ's reply to the last NS command, every line of it joined
        with ` / `; None when nothing came."""
        is_ns = lambda l: l.startswith(":NickServ!") and " NOTICE " in l
        _, first = self.wait(is_ns)
        if not first:
            return None
        lines = [first]
        self.sock.settimeout(QUIET)
        try:
            while True:
                _, more = self.wait(is_ns)
                if not more:
                    break
                lines.append(more)
        finally:
            self.sock.settimeout(STEP)
        return " / ".join(trailing(l) for l in lines)

    def close(self):
        try:
            self.send("QUIT :done")
            time.sleep(0.2)
        except OSError:
            pass
        finally:
            self.sock.close()


def trailing(line):
    """The trailing parameter of an IRC line (after ` :`), or the line."""
    return line.split(" :", 1)[1] if " :" in line else line


def cap_ls(irc):
    """`CAP LS 302`; the capability names offered."""
    irc.send("CAP LS 302")
    caps = set()
    while True:
        _, line = irc.wait(lambda l: " CAP " in l and " LS " in l)
        if not line:
            break
        fields = line.split(" ")
        more = fields[fields.index("LS") + 1] == "*"
        caps |= {c.split("=")[0] for c in trailing(line).split()}
        if not more:
            break
    return caps


class Registration:
    """What one connection's registration established, as the server said it."""

    def __init__(self):
        self.offered = set()  # CAP LS
        self.acked = set()  # CAP ACK: negotiated, not merely offered
        self.account = None  # named by 900 and confirmed by 903, else None
        self.welcomed = False  # 001 arrived
        self.nick = None  # the nick the server welcomed, which it may have changed
        self.isupport = set()  # 005 tokens
        self.error = None  # the first thing that went wrong, as received


def register_connection(irc, nick, want_caps=(), sasl=None):
    """CAP → (SASL) → NICK/USER → welcome. `sasl` is None, "external", or
    (user, password). A refusal is reported in the result, never raised."""
    reg = Registration()
    reg.offered = cap_ls(irc)
    req = [c for c in want_caps if c in reg.offered]
    if sasl:
        req.append("sasl")
    if req:
        irc.send("CAP REQ :" + " ".join(req))
        _, line = irc.wait(lambda l: " CAP " in l and (" ACK " in l or " NAK " in l))
        if line and " ACK " in line:
            reg.acked = set(trailing(line).split())
    if sasl:
        if "sasl" not in reg.acked:
            reg.error = "the server did not acknowledge the sasl capability"
        else:
            sasl_exchange(irc, reg, sasl)
    # NICK last, so the server's answer to it can only land in the wait below.
    irc.send(f"NICK {nick}")
    irc.send("USER mu 0 * :mu-irc-gateway puppet slot")
    irc.send("CAP END")
    _, line = irc.numeric("001", *REG_FAIL)
    if line and " 001 " in line:
        reg.welcomed = True
        reg.nick = line.split(" ")[2]
        seen, _ = irc.wait(lambda l: " 376 " in l or " 422 " in l)
        for l in seen:
            if " 005 " in l:
                reg.isupport |= {t.split("=")[0] for t in l.split(" ")[3:] if not t.startswith(":")}
    elif reg.error is None:
        reg.error = "registration refused: " + (line or "no welcome from the server")
    return reg


def sasl_exchange(irc, reg, sasl):
    mech = "EXTERNAL" if sasl == "external" else "PLAIN"
    irc.send(f"AUTHENTICATE {mech}")
    _, line = irc.wait(lambda l: l.startswith("AUTHENTICATE") or any(f" {c} " in l for c in SASL_FAIL))
    if not line or not line.startswith("AUTHENTICATE"):
        reg.error = "SASL refused: " + (trailing(line) if line else "no answer to AUTHENTICATE")
        return
    if sasl == "external":
        irc.send("AUTHENTICATE +")
    else:
        user, password = sasl
        irc.send("AUTHENTICATE " + base64.b64encode(f"{user}\0{user}\0{password}".encode()).decode())
    named = None
    while True:
        _, line = irc.numeric("900", "903", *SASL_FAIL)
        if not line:
            reg.error = "SASL: no answer"
            return
        if " 900 " in line:
            # :server 900 <nick> <nick!user@host> <account> :You are now logged in as <account>
            fields = line.split(" ")
            named = fields[4] if len(fields) > 4 else None
            continue
        if " 903 " in line and named:
            reg.account = named
        elif " 903 " in line:
            reg.error = "SASL: 903 without a 900 naming the account"
        else:
            reg.error = "SASL failed: " + trailing(line)
        return


def logged_in_as(reg, slot):
    """The server named this slot as the account, and let the connection in."""
    return reg.welcomed and reg.account is not None and reg.account.lower() == slot.lower()


def always_on_state(irc):
    """`NS GET always-on` as the logged-in account: ("on" | "off" | None, reply).
    None is an answer the script does not understand, or none at all."""
    irc.send("NS GET always-on")
    reply = irc.nickserv()
    if reply is None:
        return None, None
    low = reply.lower()
    if "client is not always-on" in low:
        return "off", reply
    if "client is always-on" in low:
        return "on", reply
    return None, reply


def whox_rows(lines):
    """(nick, account) per `354` reply to `WHO <chan> %tcuhsnfa,7`:
    :server 354 <me> 7 <chan> <user> <host> <server> <nick> <flags> <account>"""
    rows = []
    for l in lines:
        f = l.split(" ")
        if len(f) >= 11 and f[1] == "354":
            rows.append((f[8], f[10].lstrip(":")))
    return rows


# ───────────────────────────────── the slots ────────────────────────────────


def slots_of(args):
    return [f"{args.prefix}-{n}" for n in range(1, args.count + 1)]


def cert_paths(args, slot):
    return os.path.join(args.dir, f"{slot}.crt"), os.path.join(args.dir, f"{slot}.key")


def openssl(*argv):
    return subprocess.run(["openssl", *argv], check=True, capture_output=True, text=True).stdout


def fingerprint(crt):
    out = openssl("x509", "-in", crt, "-noout", "-fingerprint", "-sha256")
    return out.split("=", 1)[1].strip().replace(":", "").lower()


def pair_matches(crt, key):
    """The certificate and the key carry the same public key."""
    return openssl("x509", "-in", crt, "-noout", "-pubkey") == openssl("pkey", "-in", key, "-pubout")


FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "fixtures")


def fixture_fingerprints():
    """The crate's committed test slot certificates: they identify nothing
    anywhere and must never reach a server (tests/fixtures/README.md)."""
    fps = set()
    for name in ("slot-a.pem", "slot-b.pem"):
        path = os.path.join(FIXTURES, name)
        if os.path.exists(path):
            fps.add(fingerprint(path))
    return fps


def unreadable(e):
    """What openssl said, one line, for a pair it could not read."""
    said = (e.stderr or "").strip().splitlines()
    return said[-1] if said else str(e)


def pair_ready(args, slot, fixtures):
    """The slot's (crt, key), or None after saying what is wrong with it."""
    crt, key = cert_paths(args, slot)
    missing = [p for p in (crt, key) if not os.path.exists(p)]
    if missing:
        print(f"{slot}: missing {', '.join(missing)}" + (" — run `certs` first" if len(missing) == 2 else " — a partial pair; restore it or remove the other half and run `certs`"))
        return None
    try:
        fp = fingerprint(crt)
        matches = pair_matches(crt, key)
    except subprocess.CalledProcessError as e:
        print(f"{slot}: openssl cannot read the pair ({unreadable(e)}); restore it, or remove both halves and run `certs`")
        return None
    if fp in fixtures:
        print(f"{slot}: {crt} is a test fixture certificate; mint real ones with `certs`")
        return None
    if not matches:
        print(f"{slot}: {crt} and {key} do not match")
        return None
    return crt, key


CONNECT = Irc.connect  # the tests put a scripted connection here


def connect(args, cert=None, key=None):
    try:
        return CONNECT(args.server, args.ca, cert, key, not args.no_hostname_check)
    except (OSError, ValueError) as e:  # ssl errors are OSErrors
        sys.exit(f"cannot connect to {args.server}" + (f" as {os.path.basename(cert)}" if cert else "") + f": {e}")


def external_login(args, crt, key, slot, want_caps=()):
    """Connect with the slot's certificate and log in over SASL EXTERNAL."""
    irc = connect(args, crt, key)
    try:
        return register_connection(irc, slot, want_caps, sasl="external"), irc
    except BaseException:
        irc.close()
        raise


def provisioned(args, crt, key, slot):
    """Whether the certificate logs in as the slot; what the server said; and
    the account it logged in as when that was some other one."""
    reg, irc = external_login(args, crt, key, slot)
    irc.close()
    if logged_in_as(reg, slot):
        return True, f"the certificate logs in as {slot}", None
    if reg.account:
        return False, f"the certificate logs in as {reg.account}, not {slot}: it is attached to the wrong account", reg.account
    return False, reg.error or "not logged in", None


# ────────────────────────────────── commands ─────────────────────────────────


BEFORE_PUBLISH = lambda: None  # the tests race another run in here


def mint(slot, crt, key, days):
    """Mint a pair beside the final names and publish it by hard link, which
    fails if the name exists: two runs minting the same slot at once cannot
    overwrite each other, and the one whose key lands second keeps nothing."""
    tag = f".minting-{os.getpid()}"
    tmp_crt, tmp_key = crt + tag, key + tag
    openssl(
        "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
        "-keyout", tmp_key, "-out", tmp_crt, "-days", str(days),
        "-subj", f"/CN=mu-irc-gateway slot {slot}",
    )
    os.chmod(tmp_key, 0o600)
    BEFORE_PUBLISH()
    try:
        try:
            os.link(tmp_key, key)  # the key is the claim on the slot
        except FileExistsError:
            return False
        try:
            os.link(tmp_crt, crt)
        except FileExistsError:
            os.remove(key)
            return False
        return True
    finally:
        os.remove(tmp_key)
        os.remove(tmp_crt)


def cmd_certs(args):
    os.makedirs(args.dir, mode=0o700, exist_ok=True)
    failures = 0
    for slot in slots_of(args):
        crt, key = cert_paths(args, slot)
        have = [p for p in (crt, key) if os.path.exists(p)]
        if len(have) == 2:
            try:
                if pair_matches(crt, key):
                    print(f"{slot}: exists, kept  {fingerprint(crt)}")
                else:
                    print(f"{slot}: {crt} and {key} do not match; nothing overwritten — remove both to mint again")
                    failures += 1
            except subprocess.CalledProcessError as e:
                print(f"{slot}: openssl cannot read the pair ({unreadable(e)}); nothing overwritten — restore it, or remove both to mint again")
                failures += 1
            continue
        if have:
            print(f"{slot}: only {have[0]} exists; nothing overwritten — restore its pair, or remove it to mint again")
            failures += 1
            continue
        if mint(slot, crt, key, args.days):
            print(f"{slot}: minted        {fingerprint(crt)}")
        else:
            print(f"{slot}: a pair appeared while minting (another run?); kept theirs, mine discarded")
            failures += 1
    if failures:
        print(f"\n{failures} slot(s) unusable; the fingerprints above are what the server will know the others by.")
    else:
        print(f"\n{args.count} slot certificate(s) in {args.dir}; the fingerprints above are what the server will know them by.")
    return 1 if failures else 0


def cmd_register(args):
    if bool(args.oper) != bool(args.oper_pass_file):
        sys.exit("--oper and --oper-pass-file go together")
    fixtures = fixture_fingerprints()
    failures = 0
    todo = []
    for slot in slots_of(args):
        pair = pair_ready(args, slot, fixtures)
        if not pair:
            failures += 1
            continue
        ok, why, other = provisioned(args, *pair, slot)
        if ok:
            print(f"{slot}: already provisioned ({why})")
        elif other:
            print(f"{slot}: {why} — not registering under it")
            failures += 1
        else:
            todo.append((slot, pair))
    if args.oper:
        failures += register_as_oper(args, todo)
    else:
        for slot, pair in todo:
            failures += register_open(args, slot, pair)
    for slot, pair in todo:
        ok, why, _ = provisioned(args, *pair, slot)
        print(f"{slot}: {'provisioned' if ok else 'NOT provisioned'} — {why}" + ("" if ok else f" (fingerprint {fingerprint(pair[0])})"))
        failures += 0 if ok else 1
    return 1 if failures else 0


def register_open(args, slot, pair):
    """The slot registers itself from its own TLS connection; Ergo attaches
    the certificate it presented. Returns the number of failures (0 or 1)."""
    crt, key = pair
    irc = connect(args, crt, key)
    try:
        reg = register_connection(irc, slot)
        if not reg.welcomed:
            print(f"{slot}: cannot connect as {slot} to register: {reg.error}" + (f" — if the account exists without its certificate, attach it: `NS CERT ADD {fingerprint(crt)}` as {slot}, or `NS CERT ADD {slot} {fingerprint(crt)}` as an operator" if "433" in (reg.error or "") else ""))
            return 1
        irc.send(f"NS REGISTER {secrets.token_urlsafe(24)}")
        reply = irc.nickserv()
        print(f"{slot}: REGISTER -> {reply}")
        if reply is None or "already" in reply.lower():
            if reply is not None:
                print(f"{slot}: exists without its certificate — `NS CERT ADD {fingerprint(crt)}` as {slot}, or `NS CERT ADD {slot} {fingerprint(crt)}` as an operator")
            return 1
        irc.send("NS CERT ADD")
        print(f"{slot}: CERT ADD -> {irc.nickserv()}")
        return 0
    finally:
        irc.close()


def register_as_oper(args, todo):
    """One connection as the operator creates the accounts and attaches the
    fingerprints. Returns the number of failures."""
    if not todo:
        return 0
    with open(args.oper_pass_file) as f:
        oper_pass = f.read().strip()
    irc = connect(args)
    try:
        reg = register_connection(irc, f"{args.prefix}-prov{os.getpid() % 1000}")
        if not reg.welcomed:
            print(f"cannot connect to provision: {reg.error}")
            return len(todo)
        irc.send(f"OPER {args.oper} {oper_pass}")
        _, line = irc.numeric("381", "464", "491")
        if not line or " 381 " not in line:
            print(f"OPER {args.oper} failed: {trailing(line) if line else 'no reply'}")
            return len(todo)
        for slot, (crt, _) in todo:
            irc.send(f"NS SAREGISTER {slot} {secrets.token_urlsafe(24)}")
            print(f"{slot}: SAREGISTER -> {irc.nickserv()}")
            irc.send(f"NS CERT ADD {slot} {fingerprint(crt)}")
            print(f"{slot}: CERT ADD -> {irc.nickserv()}")
        return 0
    finally:
        irc.close()


REQUIRED_CAPS = ("account-tag", "extended-join", "account-notify", "sasl")


def cmd_verify(args):
    fixtures = fixture_fingerprints()
    failures = 0
    for slot in slots_of(args):
        pair = pair_ready(args, slot, fixtures)
        if not pair:
            failures += 1
            continue
        reg, irc = external_login(args, *pair, slot, want_caps=REQUIRED_CAPS)
        try:
            state, reply = always_on_state(irc) if logged_in_as(reg, slot) else (None, None)
        finally:
            irc.close()
        problems = []
        if not logged_in_as(reg, slot):
            problems.append(f"the certificate logs in as {reg.account}, not {slot}" if reg.account else (reg.error or "not logged in"))
        missing = [c for c in REQUIRED_CAPS if c not in reg.acked]
        if "WHOX" not in reg.isupport:
            missing.append("WHOX")
        if missing:
            problems.append("not negotiated: " + ", ".join(missing))
        if state == "on":
            problems.append("always-on: the seat would outlive the puppet (`NS SET always-on false` as the slot)")
        elif state is None and logged_in_as(reg, slot):
            problems.append(f"always-on state unknown; NickServ said: {reply!r}")
        if problems:
            failures += 1
            print(f"{slot}: FAIL  " + "; ".join(problems))
        else:
            print(f"{slot}: ok    logs in as {reg.account}; {', '.join(REQUIRED_CAPS)} negotiated, WHOX offered, not always-on")
    if failures:
        print(f"\n{failures} slot(s) not ready; the gateway refuses to run a pool the server cannot attribute.")
    else:
        print(f"\nall {args.count} slot(s) log in over SASL EXTERNAL as their own account; the server negotiates what the pool needs.")
    return 1 if failures else 0


def cmd_config(args):
    print(
        "[irc.puppets]\n"
        "enabled        = true\n"
        f'slot_certs_dir = "{os.path.abspath(args.dir)}"\n'
        f'slot_prefix    = "{args.prefix}"\n'
        f"max            = {args.count}\n"
        "# min_age_secs = 60          # a peer is this old before it is worth a connection\n"
        "# slot_idle_secs = 3600      # an idle lease may be taken after this (default an hour)\n"
        "# departure_wait_secs = 10   # reuse wait after a puppet's connection ends\n"
        "\n# [irc] tls = true is required: a slot's credential is its certificate."
    )
    return 0


def cmd_smoke(args):
    nick = args.nick or f"probe-{os.getpid() % 10000}"
    irc = connect(args)
    try:
        reg = register_connection(irc, nick, want_caps=("extended-join", "account-tag"))
        if not reg.welcomed:
            sys.exit(f"cannot connect as {nick}: {reg.error}")
        irc.send(f"JOIN {args.lobby}")
        seen, end = irc.numeric("366", "403", "405", "471", "473", "474", "475", "477")
        if not end or " 366 " not in end:
            sys.exit(f"JOIN {args.lobby} refused: {trailing(end) if end else 'no reply'}")
        names = [trailing(l) for l in seen if " 353 " in l]
        print(f"NAMES {args.lobby}: {' '.join(names)}")
        irc.send(f"WHO {args.lobby} %tcuhsnfa,7")
        seen, end = irc.numeric("315")
        if not end or " 315 " not in end:
            sys.exit(f"WHO {args.lobby}: " + (trailing(end) if end else "no end of the WHO list came"))
        rows = whox_rows(seen)
        if not any(n.lower() == reg.nick.lower() for n, _ in rows):
            sys.exit(f"WHO {args.lobby}: no WHOX row for {reg.nick} itself among {len(seen) - 1} line(s) — the server did not answer the WHOX form, and the pool cannot run on it")
        width = max(len(n) for n, _ in rows)
        for n, a in rows:
            print(f"  {n:<{width}}  account={a}")
        irc.send(f"PART {args.lobby}")
    finally:
        irc.close()
    return 0


def slot_count(text):
    n = int(text)
    if n < 1:
        raise argparse.ArgumentTypeError(f"{text}: a pool has at least one slot")
    return n


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    def common(sp, server=True, dir_=True):
        if server:
            sp.add_argument("--server", required=True, help="host:port (TLS); an IPv6 address in brackets")
            sp.add_argument("--ca", help="PEM bundle the server's certificate chains to; omit for a public CA")
            sp.add_argument("--no-hostname-check", action="store_true", help="skip the server-name check (a throwaway by IP)")
        if dir_:
            sp.add_argument("--dir", required=True, help="the slots directory ([irc.puppets] slot_certs_dir)")
            sp.add_argument("--prefix", default="cc", help="slot prefix ([irc.puppets] slot_prefix)")
            sp.add_argument("--count", type=slot_count, required=True, help="number of slots ([irc.puppets] max), at least 1")

    sp = sub.add_parser("certs", help="mint a certificate per slot")
    common(sp, server=False)
    sp.add_argument("--days", type=int, default=365)
    sp.set_defaults(run=cmd_certs)
    sp = sub.add_parser("register", help="create the slot accounts and attach their certificates")
    common(sp)
    sp.add_argument("--oper", help="operator name, for a server with registration closed (NS SAREGISTER)")
    sp.add_argument("--oper-pass-file", help="file holding the operator password")
    sp.set_defaults(run=cmd_register)
    sp = sub.add_parser("verify", help="log in as every slot over SASL EXTERNAL and check the capabilities")
    common(sp)
    sp.set_defaults(run=cmd_verify)
    sp = sub.add_parser("config", help="print the [irc.puppets] lines")
    common(sp, server=False)
    sp.set_defaults(run=cmd_config)
    sp = sub.add_parser("smoke", help="join the lobby as a probe and show who is there, and as whom")
    common(sp, dir_=False)
    sp.add_argument("--lobby", default="#mu")
    sp.add_argument("--nick")
    sp.set_defaults(run=cmd_smoke)
    args = p.parse_args(argv)
    return args.run(args)


if __name__ == "__main__":
    sys.exit(main())
