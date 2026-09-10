#!/usr/bin/env python3
"""Mechanical AGENTS.md architecture-invariant audit (bead
mu-review-gate-seam-reviewers-9vkbt.6).

Pattern-matches the *declared* invariant violation shapes (scripts/invariants.toml)
against the repo and reports every site. It is a cheap mechanical pre-filter for
the review gate's conformance seat, NOT a replacement for it: only shapes a regex
or a path glob can see are checked here; the invariants with no reliable pattern
(see invariants.toml) stay the seat's job.

Standalone contract: standard library only (Python 3.11+, tomllib), no mu
dependency. It reads a rules TOML and a repo tree and writes lines to stdout, so
it can be lifted into any repo — drop in an invariants.toml describing that
repo's shapes and run it.

Revision pinning: --changed-from BASE diffs BASE..REV, where REV is --to (default
@ under jj, HEAD under git). The changed paths come from that diff and each file's
CONTENT is read AT REV (jj file show -r REV / git show REV:PATH), never from the
working tree, so a pinned commit is audited exactly as it was. forbidden_path
rules fire on the same REV path list. --all always scans the WORKING TREE.

Severity vocabulary: rules and the INVARIANT output lines speak high|medium|low.
--gate-severity re-renders the emitted severity in the review gate's own
vocabulary (blocker|should-fix|note) so downstream wiring reuses one mapping
instead of inventing another; the rules file is unchanged either way.

The baseline ratchet (scripts/invariants.baseline): each line is one accepted
PRE-EXISTING site as `<rule id> <path> <sha256[:16] of the stripped matched
line>` (no line numbers, so a site survives edits above it), with an optional
fourth field `<count>` recording how many identical stripped lines are accepted
(absent = 1, so old 3-field lines still parse). The ratchet is OCCURRENCE-AWARE:
current sites are grouped by (rule, path, digest) and their COUNT compared to the
baseline's. As many copies as the baseline records PASS; any surplus copies FAIL
as NEW (reported with their line numbers); if fewer copies remain than baselined,
the shortfall is reported "fixed: N of M" (the run still passes). A baseline line
is a debt marker, never a free pass: fix the site and drop/decrement the line, or
bead it and keep it. --update-baseline rewrites the file from the current sites,
writing the count field only when it exceeds 1.

Exit: 0 when every current site is in the baseline; 1 when any is not; 2 on a
usage or rules error.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tomllib
from typing import NoReturn

MAX_BYTES = 2 * 1024 * 1024          # files larger than this are skipped
BINARY_SNIFF_BYTES = 8 * 1024        # a NUL in the first 8 KB means "binary"
# Directory basenames never walked under --all: VCS internals and build output.
# The rules' exclude globs also cover these; pruning here is the speed win.
PRUNE_DIRS = {".git", ".jj", "target", "node_modules"}

# The one mapping from a rule's severity (high|medium|low) to the review gate's
# vocabulary (blocker|should-fix|note). Kept here so the future gate wiring reuses
# it rather than inventing its own; --gate-severity renders output through it. An
# unknown value passes through unchanged so a new severity can never silently drop.
GATE_SEVERITY = {"high": "blocker", "medium": "should-fix", "low": "note"}


def gate_severity(sev):
    return GATE_SEVERITY.get(sev, sev)


def die(msg, code=2) -> NoReturn:
    sys.stderr.write("invariant-audit: %s\n" % msg)
    sys.exit(code)


# --- glob → regex ----------------------------------------------------------
# Path globs, POSIX-slash semantics: `*` and `?` do NOT cross `/`; `**/` matches
# any number of leading directory segments (including zero); a trailing `**`
# matches everything below. Python's stdlib has no path-aware glob matcher we can
# rely on across 3.11–3.13, so translate to an anchored regex once and cache it.
_GLOB_CACHE = {}


def _glob_re(pattern):
    r = _GLOB_CACHE.get(pattern)
    if r is not None:
        return r
    i, n, out = 0, len(pattern), []
    while i < n:
        c = pattern[i]
        i += 1
        if c == "*":
            if i < n and pattern[i] == "*":
                i += 1
                if i < n and pattern[i] == "/":
                    i += 1
                    out.append("(?:[^/]+/)*")   # **/  → zero or more dir segments
                else:
                    out.append(".*")             # **   → anything, incl. slashes
            else:
                out.append("[^/]*")              # *    → within one segment
        elif c == "?":
            out.append("[^/]")
        else:
            out.append(re.escape(c))
    r = re.compile("(?s:%s)\\Z" % "".join(out))
    _GLOB_CACHE[pattern] = r
    return r


def glob_match(pattern, path):
    return _glob_re(pattern).match(path) is not None


def any_glob(patterns, path):
    return any(glob_match(p, path) for p in patterns)


# --- rules -----------------------------------------------------------------
class Rule:
    __slots__ = ("id", "title", "kind", "patterns", "regexes",
                 "include", "exclude", "severity", "why")

    def __init__(self, raw, idx):
        where = "rule #%d" % (idx + 1)
        try:
            self.id = str(raw["id"])
            self.title = str(raw["title"])
            self.kind = str(raw["kind"])
        except KeyError as e:
            die("%s missing required field %s" % (where, e))
        if self.kind not in ("regex", "forbidden_path"):
            die("%s (%s): unknown kind %r (want regex|forbidden_path)"
                % (where, self.id, self.kind))
        pat = raw.get("pattern")
        if pat is None:
            die("%s (%s) missing required field 'pattern'" % (where, self.id))
        self.patterns = [pat] if isinstance(pat, str) else [str(p) for p in pat]
        if not self.patterns:
            die("%s (%s): empty pattern" % (where, self.id))
        self.include = [str(g) for g in raw.get("include", [])]  # empty = all
        self.exclude = [str(g) for g in raw.get("exclude", [])]
        self.severity = str(raw.get("severity", "medium"))
        self.why = str(raw.get("why", ""))
        self.regexes = []
        if self.kind == "regex":
            for p in self.patterns:
                try:
                    self.regexes.append(re.compile(p))
                except re.error as e:
                    die("%s (%s): bad regex %r: %s" % (where, self.id, p, e))

    def wants_path(self, path):
        if self.include and not any_glob(self.include, path):
            return False
        if self.exclude and any_glob(self.exclude, path):
            return False
        return True


def load_rules(path):
    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
    except FileNotFoundError:
        die("rules file not found: %s" % path)
    except (tomllib.TOMLDecodeError, OSError) as e:
        die("cannot read rules file %s: %s" % (path, e))
    raw_rules = data.get("rule")
    if not isinstance(raw_rules, list) or not raw_rules:
        die("rules file %s has no [[rule]] entries" % path)
    return [Rule(r, i) for i, r in enumerate(raw_rules)]


# --- repo / file helpers ---------------------------------------------------
def discover_root(start):
    d = os.path.abspath(start)
    while True:
        if os.path.isdir(os.path.join(d, ".jj")) or \
           os.path.isdir(os.path.join(d, ".git")):
            return d
        parent = os.path.dirname(d)
        if parent == d:
            return None
        d = parent


def rel_path(root, p):
    ap = p if os.path.isabs(p) else os.path.join(root, p)
    return os.path.relpath(ap, root).replace(os.sep, "/")


def is_binary(abspath):
    try:
        with open(abspath, "rb") as f:
            return b"\x00" in f.read(BINARY_SNIFF_BYTES)
    except OSError:
        return True


def walk_repo(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in PRUNE_DIRS]
        for name in filenames:
            ap = os.path.join(dirpath, name)
            yield os.path.relpath(ap, root).replace(os.sep, "/")


def default_head(root):
    """The default far end of --changed-from: @ under jj, HEAD under git."""
    return "@" if os.path.isdir(os.path.join(root, ".jj")) else "HEAD"


def changed_paths(root, rev, to):
    is_jj = os.path.isdir(os.path.join(root, ".jj"))
    if is_jj:
        # jj --name-only is newline-separated and does NOT quote paths.
        cmd = ["jj", "diff", "--from", rev, "--to", to, "--name-only"]
    else:
        # git -z NUL-separates paths and never quotes them (the default octal-
        # quotes non-ASCII names, which would then be skipped as missing); split
        # on NUL, dropping only the element after the trailing NUL.
        cmd = ["git", "diff", "-z", "--name-only", "%s...%s" % (rev, to)]
    try:
        out = subprocess.run(cmd, cwd=root, capture_output=True)
    except OSError as e:
        die("cannot run %s: %s" % (cmd[0], e))
    if out.returncode != 0:
        die("%s failed: %s"
            % (" ".join(cmd), out.stderr.decode("utf-8", "replace").strip()))
    text = out.stdout.decode("utf-8", "surrogateescape")
    if is_jj:
        return [ln.strip() for ln in text.splitlines() if ln.strip()]
    return [p for p in text.split("\0") if p]


def _run(cmd, root):
    try:
        return subprocess.run(cmd, cwd=root, capture_output=True)
    except OSError as e:
        die("cannot run %s: %s" % (cmd[0], e))


def content_at_rev(root, rev, path):
    """File bytes at REV, or None if the path is ABSENT there.

    "Absent at REV" (a path in the diff that does not exist at the pinned
    revision — e.g. a deletion) is distinguished from a VCS FAILURE (a bogus
    rev, a broken invocation): absence returns None so the caller skips the path
    silently; a failure is a hard error (die, exit 2) so a typo never masquerades
    as a clean scan. We key absence off the stderr MESSAGE, not the numeric exit
    code — git reports a missing path with status 128 ("... does not exist ..."),
    not 1, and a bad rev reports a different message ("invalid object name",
    "exists on disk, but not in ..."), so the phrase is the reliable signal.
    """
    if os.path.isdir(os.path.join(root, ".jj")):
        cmd = ["jj", "file", "show", "-r", rev, "--", path]
        out = _run(cmd, root)
        if out.returncode == 0:
            return out.stdout
        err = out.stderr.decode("utf-8", "replace")
        if "No such path" in err:
            return None
        die("%s failed: %s" % (" ".join(cmd), err.strip()))
    # git: probe existence at REV first, so a genuinely missing path (status != 0
    # with a "does not exist" / "Not a valid object name" message) is absent while
    # anything else is a failure; only then read the bytes with `git show`.
    probe = ["git", "cat-file", "-e", "%s:%s" % (rev, path)]
    pr = _run(probe, root)
    if pr.returncode == 0:
        show = ["git", "show", "%s:%s" % (rev, path)]
        out = _run(show, root)
        if out.returncode != 0:
            die("%s failed: %s"
                % (" ".join(show), out.stderr.decode("utf-8", "replace").strip()))
        return out.stdout
    err = pr.stderr.decode("utf-8", "replace")
    if "does not exist" in err or "Not a valid object name" in err:
        return None
    die("%s failed: %s" % (" ".join(probe), err.strip()))


# --- scanning --------------------------------------------------------------
class Site:
    __slots__ = ("rule_id", "severity", "path", "line", "title", "text")

    def __init__(self, rule, path, line, text):
        self.rule_id = rule.id
        self.severity = rule.severity
        self.path = path
        self.line = line
        self.title = rule.title
        self.text = text.strip()

    @property
    def digest(self):
        return hashlib.sha256(
            self.text.encode("utf-8", "replace")).hexdigest()[:16]

    @property
    def key(self):
        return (self.rule_id, self.path, self.digest)

    def render(self, gate=False):
        sev = gate_severity(self.severity) if gate else self.severity
        return "INVARIANT %s|%s|%s:%d|%s|%s" % (
            self.rule_id, sev, self.path, self.line,
            self.title, self.text)


def scan(rules, root, paths, rev=None):
    """Yield Site objects for every match across `paths` (repo-relative).

    rev=None reads each file's existence/content from the WORKING TREE; a set rev
    reads them AT THAT REVISION (content_at_rev), so a pinned commit is audited as
    it was, not as the tree is now. forbidden_path uses the same existence source.
    """
    sites = []
    for path in paths:
        abspath = os.path.join(root, path)
        fp_rules = [r for r in rules
                    if r.kind == "forbidden_path" and r.wants_path(path)]
        regex_rules = [r for r in rules
                       if r.kind == "regex" and r.wants_path(path)]
        if not fp_rules and not regex_rules:
            continue
        # Resolve existence (and, at a rev, the raw bytes) once for this path.
        if rev is None:
            exists = os.path.isfile(abspath)
            raw_bytes = None
        else:
            raw_bytes = content_at_rev(root, rev, path)
            exists = raw_bytes is not None
        # forbidden_path rules fire on the path alone, given the file exists.
        for rule in fp_rules:
            if exists and any_glob(rule.patterns, path):
                sites.append(Site(rule, path, 0, path))
        if not regex_rules or not exists:
            continue
        # regex rules read the file once, shared across all regex rules.
        if rev is None:
            try:
                if os.path.getsize(abspath) > MAX_BYTES:
                    continue
            except OSError:
                continue
            if is_binary(abspath):
                continue
            try:
                with open(abspath, "r", encoding="utf-8", errors="replace") as f:
                    lines = f.readlines()
            except OSError:
                continue
        else:
            if len(raw_bytes) > MAX_BYTES:
                continue
            if b"\x00" in raw_bytes[:BINARY_SNIFF_BYTES]:
                continue
            lines = raw_bytes.decode("utf-8", "replace").splitlines(keepends=True)
        for rule in regex_rules:
            for lineno, raw in enumerate(lines, 1):
                if any(rx.search(raw) for rx in rule.regexes):
                    sites.append(Site(rule, path, lineno, raw))
    return sites


# --- baseline --------------------------------------------------------------
BASELINE_HEADER = (
    "# invariant_audit baseline — accepted PRE-EXISTING invariant sites.\n"
    "# One per line: <rule id> <repo-relative path> <sha256[:16] of the "
    "stripped matched line> [<count>].\n"
    "# The optional <count> is how many identical copies are accepted (absent = "
    "1); the ratchet compares per-key counts, so a surplus copy fails as new.\n"
    "# A line here is a DEBT MARKER, never a free pass: fix the site and remove "
    "the line, or bead it and keep the line.\n"
    "# Blank lines and lines starting with # are ignored. Regenerate with:\n"
    "#   python3 scripts/review-panel/invariant_audit.py --all --update-baseline\n"
)


# A baseline line is `<rule> <path> <digest16> [<count>]`, but PATHS MAY CONTAIN
# SPACES, so we parse from the ends, not by a fixed field count: rule is the first
# token; from the right, an optional all-digit count then a 16-hex digest; the
# path is everything between, with its internal whitespace preserved verbatim. The
# greedy `(.+)` backtracks only as far as the rightmost digest(+count), so a digest
# is always taken from the right.
BASELINE_LINE_RE = re.compile(
    r"^(\S+)\s+(.+)\s+([0-9a-fA-F]{16})(?:\s+(\d+))?$")


def load_baseline(path):
    """Map (rule, path, digest) -> accepted count. A 3-field line means count 1
    (backward compatible); an optional trailing count records how many identical
    stripped lines are accepted. Repeated keys accumulate. A non-comment, non-blank
    line that does not parse is a hard error (exit 2, with its line number)."""
    counts = {}
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for lineno, raw in enumerate(f, 1):
                s = raw.strip()
                if not s or s.startswith("#"):
                    continue
                m = BASELINE_LINE_RE.match(s)
                if not m:
                    die("baseline %s line %d: cannot parse %r "
                        "(want '<rule> <path> <digest16> [count]')"
                        % (path, lineno, s))
                key = (m.group(1), m.group(2), m.group(3))
                n = int(m.group(4)) if m.group(4) is not None else 1
                counts[key] = counts.get(key, 0) + n
    except FileNotFoundError:
        pass
    except OSError as e:
        die("cannot read baseline %s: %s" % (path, e))
    return counts


def write_baseline(path, sites):
    counts = {}
    for s in sites:
        counts[s.key] = counts.get(s.key, 0) + 1
    lines = []
    for key in sorted(counts):
        n = counts[key]
        # Count is omitted when 1, keeping the line 3-field backward compatible.
        lines.append("%s %s %s" % key if n == 1 else "%s %s %s %d" % (key + (n,)))
    try:
        with open(path, "w", encoding="utf-8") as f:
            f.write(BASELINE_HEADER)
            for ln in lines:
                f.write(ln + "\n")
    except OSError as e:
        die("cannot write baseline %s: %s" % (path, e))


# --- main ------------------------------------------------------------------
def build_parser():
    p = argparse.ArgumentParser(
        description="Mechanical AGENTS.md invariant audit with a baseline ratchet.")
    p.add_argument("--rules", help="rules TOML (default <root>/scripts/invariants.toml)")
    p.add_argument("--root", help="repo root (default: walk up from cwd for .jj/.git)")
    p.add_argument("--all", action="store_true", help="scan the whole repo tree")
    p.add_argument("--changed", nargs="+", metavar="FILE",
                   help="scan only these repo-relative files")
    p.add_argument("--changed-from", metavar="REV",
                   help="scan files changed vs REV (jj diff / git diff); "
                        "content is read at --to, not from the working tree")
    p.add_argument("--to", metavar="REV",
                   help="far end of --changed-from's range AND the revision file "
                        "content is read at (default @ under jj, HEAD under git); "
                        "ignored by --all, which always scans the working tree")
    p.add_argument("--gate-severity", action="store_true",
                   help="render severities in the review gate's vocabulary "
                        "(blocker|should-fix|note) instead of high|medium|low")
    p.add_argument("--baseline", help="baseline file (default <root>/scripts/invariants.baseline)")
    p.add_argument("--update-baseline", action="store_true",
                   help="rewrite the baseline from the current sites and exit 0 "
                        "(requires --all: it replaces the whole file)")
    p.add_argument("--json", action="store_true", help="emit one JSON object")
    p.add_argument("--quiet", action="store_true",
                   help="suppress the summary and the fixed-site notes")
    return p


def main(argv):
    args = build_parser().parse_args(argv)

    root = args.root or discover_root(os.getcwd())
    if not root:
        die("no repo root: pass --root or run inside a .jj/.git tree")
    root = os.path.abspath(root)
    if not os.path.isdir(root):
        die("root is not a directory: %s" % root)

    rules_path = args.rules or os.path.join(root, "scripts", "invariants.toml")
    baseline_path = args.baseline or os.path.join(root, "scripts", "invariants.baseline")
    rules = load_rules(rules_path)

    modes = [bool(args.all), bool(args.changed), bool(args.changed_from)]
    if sum(modes) != 1:
        die("choose exactly one of --all, --changed FILE..., --changed-from REV")

    # --to only means anything for --changed-from (the pinned range + read rev).
    if args.to and not args.changed_from:
        die("--to is only valid with --changed-from")

    # --update-baseline rewrites the WHOLE file from the scanned sites, so it must
    # see the whole tree; a --changed / --changed-from subset would silently drop
    # every baselined site outside the subset.
    if args.update_baseline and not args.all:
        die("--update-baseline rewrites the whole baseline and needs --all")

    if args.all:
        # --all scans the working tree, never a pinned revision.
        paths = list(walk_repo(root))
        scan_rev = None
    elif args.changed_from:
        to = args.to or default_head(root)
        paths = [rel_path(root, p)
                 for p in changed_paths(root, args.changed_from, to)]
        scan_rev = to
    else:
        paths = [rel_path(root, p) for p in args.changed]
        scan_rev = None

    sites = scan(rules, root, paths, scan_rev)

    if args.update_baseline:
        write_baseline(baseline_path, sites)
        if not args.quiet:
            print("invariant-audit: baseline rewritten from %d site(s): %s"
                  % (len({s.key for s in sites}), baseline_path))
        return 0

    baseline = load_baseline(baseline_path)
    # Group current sites by (rule, path, digest) and compare COUNTS against the
    # baseline: more current than baselined => the surplus copies are NEW; fewer
    # => some were fixed. Grouped in first-seen order; the surplus reported is the
    # copies with the highest line numbers (the later duplicates).
    current = {}
    for s in sites:
        current.setdefault(s.key, []).append(s)
    new_sites, baselined = [], 0
    for key, group in current.items():
        allowed = baseline.get(key, 0)
        baselined += min(len(group), allowed)
        if len(group) > allowed:
            group = sorted(group, key=lambda s: s.line)
            new_sites.extend(group[allowed:])
    new_sites.sort(key=lambda s: (s.path, s.line))
    # "fixed" means fewer current copies than baselined — only a whole-tree scan
    # can assert that; a --changed run saw only a subset, so it stays silent.
    # Each entry is (key, n_fixed, m_baselined).
    fixed = []
    if args.all:
        for key in sorted(baseline):
            have = len(current.get(key, []))
            if have < baseline[key]:
                fixed.append((key, baseline[key] - have, baseline[key]))
    exit_code = 1 if new_sites else 0

    def sev(s):
        return gate_severity(s.severity) if args.gate_severity else s.severity

    if args.json:
        obj = {
            "new": [{"id": s.rule_id, "severity": sev(s), "file": s.path,
                     "line": s.line, "title": s.title, "match": s.text}
                    for s in new_sites],
            "baselined": baselined,
            "fixed": [{"id": k[0], "path": k[1], "digest": k[2],
                       "fixed": n, "baselined": m} for (k, n, m) in fixed],
            "sites": len(sites),
            "exit": exit_code,
        }
        print(json.dumps(obj))
        return exit_code

    for s in new_sites:
        print(s.render(gate=args.gate_severity))
    if not args.quiet:
        for ((i, p, d), n, m) in fixed:
            print("fixed: %d of %d: %s %s %s" % (n, m, i, p, d))
        print("invariant-audit: %d new, %d baselined, %d fixed "
              "(%d site(s) across %d path(s))"
              % (len(new_sites), baselined, sum(n for (_, n, _) in fixed),
                 len(sites), len(paths)))
    return exit_code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
