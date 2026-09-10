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

The baseline ratchet (scripts/invariants.baseline): each line is one accepted
PRE-EXISTING site as `<rule id> <path> <sha256[:16] of the stripped matched
line>` (no line numbers, so a site survives edits above it). A site already in
the baseline PASSES; a NEW site (not in the baseline) FAILS the run; a baseline
entry whose site is gone is reported "fixed: remove from baseline" (the run still
passes). A baseline line is a debt marker, never a free pass: fix the site and
drop the line, or bead it and keep the line. --update-baseline rewrites the file
from the current sites.

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


def changed_paths(root, rev):
    if os.path.isdir(os.path.join(root, ".jj")):
        cmd = ["jj", "diff", "--from", rev, "--to", "@", "--name-only"]
    else:
        cmd = ["git", "diff", "--name-only", "%s...HEAD" % rev]
    try:
        out = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
    except OSError as e:
        die("cannot run %s: %s" % (cmd[0], e))
    if out.returncode != 0:
        die("%s failed: %s" % (" ".join(cmd), out.stderr.strip()))
    return [ln.strip() for ln in out.stdout.splitlines() if ln.strip()]


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

    def render(self):
        return "INVARIANT %s|%s|%s:%d|%s|%s" % (
            self.rule_id, self.severity, self.path, self.line,
            self.title, self.text)


def scan(rules, root, paths):
    """Yield Site objects for every match across `paths` (repo-relative)."""
    sites = []
    for path in paths:
        abspath = os.path.join(root, path)
        # forbidden_path rules fire on the path alone (no content read).
        for rule in rules:
            if rule.kind != "forbidden_path" or not rule.wants_path(path):
                continue
            if os.path.isfile(abspath) and any_glob(rule.patterns, path):
                sites.append(Site(rule, path, 0, path))
        # regex rules read the file once, shared across all regex rules.
        regex_rules = [r for r in rules
                       if r.kind == "regex" and r.wants_path(path)]
        if not regex_rules:
            continue
        if not os.path.isfile(abspath):
            continue
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
        for rule in regex_rules:
            for lineno, raw in enumerate(lines, 1):
                if any(rx.search(raw) for rx in rule.regexes):
                    sites.append(Site(rule, path, lineno, raw))
    return sites


# --- baseline --------------------------------------------------------------
BASELINE_HEADER = (
    "# invariant_audit baseline — accepted PRE-EXISTING invariant sites.\n"
    "# One per line: <rule id> <repo-relative path> <sha256[:16] of the "
    "stripped matched line>.\n"
    "# A line here is a DEBT MARKER, never a free pass: fix the site and remove "
    "the line, or bead it and keep the line.\n"
    "# Blank lines and lines starting with # are ignored. Regenerate with:\n"
    "#   python3 scripts/review-panel/invariant_audit.py --all --update-baseline\n"
)


def load_baseline(path):
    keys = set()
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for raw in f:
                s = raw.strip()
                if not s or s.startswith("#"):
                    continue
                parts = s.split()
                if len(parts) != 3:
                    continue
                keys.add((parts[0], parts[1], parts[2]))
    except FileNotFoundError:
        pass
    except OSError as e:
        die("cannot read baseline %s: %s" % (path, e))
    return keys


def write_baseline(path, sites):
    lines = sorted({"%s %s %s" % k for k in {s.key for s in sites}})
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
                   help="scan files changed vs REV (jj diff / git diff)")
    p.add_argument("--baseline", help="baseline file (default <root>/scripts/invariants.baseline)")
    p.add_argument("--update-baseline", action="store_true",
                   help="rewrite the baseline from the current sites and exit 0")
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

    if args.all:
        paths = list(walk_repo(root))
    elif args.changed_from:
        paths = [rel_path(root, p) for p in changed_paths(root, args.changed_from)]
    else:
        paths = [rel_path(root, p) for p in args.changed]

    sites = scan(rules, root, paths)

    if args.update_baseline:
        write_baseline(baseline_path, sites)
        if not args.quiet:
            print("invariant-audit: baseline rewritten from %d site(s): %s"
                  % (len({s.key for s in sites}), baseline_path))
        return 0

    baseline = load_baseline(baseline_path)
    new_sites, seen_keys = [], set()
    for s in sites:
        seen_keys.add(s.key)
        if s.key not in baseline:
            new_sites.append(s)
    # "fixed" means a baselined site no longer exists — only a whole-tree scan
    # can assert that; a --changed run saw only a subset, so it stays silent.
    fixed = sorted(baseline - seen_keys) if args.all else []
    exit_code = 1 if new_sites else 0

    if args.json:
        obj = {
            "new": [{"id": s.rule_id, "severity": s.severity, "file": s.path,
                     "line": s.line, "title": s.title, "match": s.text}
                    for s in new_sites],
            "baselined": len(seen_keys & baseline),
            "fixed": [{"id": i, "path": p, "digest": d} for (i, p, d) in fixed],
            "sites": len(sites),
            "exit": exit_code,
        }
        print(json.dumps(obj))
        return exit_code

    for s in new_sites:
        print(s.render())
    if not args.quiet:
        for (i, p, d) in fixed:
            print("fixed: remove from baseline: %s %s %s" % (i, p, d))
        print("invariant-audit: %d new, %d baselined, %d fixed "
              "(%d site(s) across %d path(s))"
              % (len(new_sites), len(seen_keys & baseline), len(fixed),
                 len(sites), len(paths)))
    return exit_code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
