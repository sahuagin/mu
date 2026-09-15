#!/usr/bin/env python3
"""invariant-audit.py — enumerate every site of a violation shape; ratchet the count.

bead: mu-invariant-audit-ratchet-8vfks. A correction applied to one site of an
anti-pattern leaves the others, and whatever pattern exists in the code is what
the next change copies; documentation in AGENTS.md did not stop either. This
tool makes the invariant mechanical: `.invariants.toml` describes each one as a
violation SHAPE (a regex over a path scope, or a bare path glob) plus the number
of sites the repo has today (`baseline`). The audit lists every site and fails
when any count rises above its baseline. Baselines only go down: when a PR
removes sites, lower the number in the same PR (the audit prints the hint).

Usage:
  invariant-audit.py                 audit; exit 1 on any count above baseline
  invariant-audit.py --report        list every site, never fail
  invariant-audit.py --self-test     run the built-in fixture suite
  invariant-audit.py --config PATH   shapes file (default <root>/.invariants.toml)
  invariant-audit.py --root PATH     repo root (default: parent of scripts/)
  invariant-audit.py --base REV      revision whose shapes file pins the baselines
                                     (default $MU_INVARIANTS_BASE or "main")
  invariant-audit.py --base-config PATH   that file, already extracted (CI)
  invariant-audit.py --no-base       use the checkout's own baselines (first commit,
                                     push to main, throwaway repos)

Baselines are pinned to BASE, the same way scripts/ai-review.sh reads the
AGENTS.md invariants at BASE (mu-rjai): a PR cannot raise a baseline in the
commit that adds the violation. The effective ceiling for an invariant is the
lower of BASE's and the checkout's baseline; an invariant absent from BASE is
new and initialises at the checkout's value. The SHAPE is pinned as well as
the number: when a checkout changes an invariant's kind/paths/exclude/pattern/
crate, BASE's shape is still counted against this checkout and held to BASE's
ceiling, so narrowing a shape cannot hide a new site; a deliberate widening
just has to record its own exact count. Fewer sites than the checkout's
baseline is also a failure in gate mode — lower the number in the same PR —
so the recorded count is always the true one. An invariant present at BASE
may not silently disappear: keep its entry with `retired = true` (and say why
in `rule`) — a retired entry is not counted, and once BASE carries the
retirement the entry may be deleted. Exit codes: 0 clean, 1 ratchet failure,
2 the audit could not run (bad shapes file, bad regex, unreadable scoped file,
BASE requested but unresolvable). Gate wiring (`just ci`, CI, pre-pr-check)
is a separate increment (AGENTS.md invariant 5); until then this is the
on-demand seam behind `just invariants`.

Shapes file:
  [settings]
  exclude = [".git/**", ".jj/**", "target/**"]     # default excludes, overridable
  [[invariant]]
  id = "short-id"                                  # stable name, used in output
  rule = "the sentence from AGENTS.md"             # printed with every violation
  kind = "content"                                 # "path", or "cargo-dependency"
  paths = ["crates/**/*.rs"]                       # globs, root-relative: `x.md` is
                                                   # the root file, `**/x.md` any x.md
  exclude = ["**/tests/**"]                        # optional, per invariant
  pattern = 'regex'                                # content kind only
  crate = "mu-core"                                # cargo-dependency kind only: the
                                                   # manifests in `paths` are parsed and
                                                   # any dependency named or renamed
                                                   # (package = ...) to it is a site
  ignore_case = false                              # content kind only
  skip_cfg_test = false                            # content kind only: drop matches inside a
                                                   # `#[cfg(test)]`-attributed item or module
                                                   # (nested ones included); the skipped count
                                                   # is printed next to the counted one
  baseline = 0                                     # sites the repo has today

Stdlib only (Python 3.11+ for tomllib); no rg dependency, so CI runners and
the review panel can both run it.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

DEFAULT_EXCLUDES = [".git/**", ".jj/**", "target/**"]


class AuditError(Exception):
    """The audit could not run to completion; the gate must not pass."""


def _glob_to_re(pattern: str) -> re.Pattern:
    """Path.glob semantics as a regex over a posix relative path: `*` and `?`
    never cross `/`, `**` spans directories, `[seq]` / `[!seq]` are character
    classes (never matching `/`). Unsupported or unterminated syntax is an
    AuditError rather than a silently narrower scope."""
    out, i, n = [], 0, len(pattern)
    while i < n:
        c = pattern[i]
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?"); i += 3
        elif pattern.startswith("**", i):
            out.append(".*"); i += 2
        elif c == "*":
            out.append("[^/]*"); i += 1
        elif c == "?":
            out.append("[^/]"); i += 1
        elif c == "[":
            j = pattern.find("]", i + 2 if pattern.startswith("[!", i) or pattern.startswith("[]", i) else i + 1)
            if j < 0:
                raise AuditError(f"glob {pattern!r}: unterminated character class")
            body = pattern[i + 1:j]
            neg = body.startswith("!")
            if neg:
                body = body[1:]
            if "/" in body:
                raise AuditError(f"glob {pattern!r}: '/' inside a character class")
            body = body.replace("\\", "\\\\").replace("]", "\\]").replace("^", "\\^")
            out.append("[^/" + body + "]" if neg else "[" + body + "]")
            i = j + 1
        else:
            out.append(re.escape(c)); i += 1
    try:
        return re.compile("^" + "".join(out) + "$")
    except re.error as e:
        raise AuditError(f"glob {pattern!r}: cannot translate: {e}") from e


def _matches(rel: str, patterns: list[str]) -> bool:
    """Root-relative only: `Cargo.toml` is the root manifest, `**/Cargo.toml`
    is every manifest. No basename fallback — a slashless exclude must not
    silently widen to the whole tree."""
    return any(_glob_to_re(pat).match(rel) for pat in patterns)


def _walk(root: Path, excludes: list[str]) -> list[str]:
    """Every file under root as a posix relative path, pruning excluded
    directories. Path.glob swallows PermissionError while descending, which
    would count an unreadable subtree as zero sites; os.walk with onerror makes
    that an AuditError instead — a gate that cannot see part of its scope must
    not pass. Exclude an unreadable directory in [settings] to scan around it."""
    files: list[str] = []

    def bad(err: OSError) -> None:
        rel = os.path.relpath(err.filename, root).replace(os.sep, "/") if err.filename else "?"
        raise AuditError(f"cannot enumerate scoped directory {rel}: {err.strerror or err}") from err

    for dirpath, dirnames, filenames in os.walk(root, onerror=bad):
        rel_dir = os.path.relpath(dirpath, root).replace(os.sep, "/")
        rel_dir = "" if rel_dir == "." else rel_dir + "/"
        dirnames[:] = sorted(d for d in dirnames if not _matches(rel_dir + d, excludes) and not _matches(rel_dir + d + "/", excludes))
        # os.walk does not descend directory symlinks; a scoped subtree behind one
        # would silently count as nothing. Fail closed: exclude the link in
        # [settings] to scan around it, or replace it with a real directory.
        for d in dirnames:
            if os.path.islink(os.path.join(dirpath, d)):
                raise AuditError(f"scoped directory {rel_dir + d} is a symlink; the audit does not follow directory symlinks (exclude it in [settings] or replace it)")
        for f in sorted(filenames):
            rel = rel_dir + f
            if not _matches(rel, excludes):
                files.append(rel)
    return files


_WALK_CACHE: dict[tuple[str, tuple[str, ...]], list[str]] = {}


def _files(root: Path, globs: list[str], excludes: list[str], global_excludes: list[str]) -> list[Path]:
    key = (str(root), tuple(global_excludes))
    if key not in _WALK_CACHE:
        _WALK_CACHE[key] = _walk(root, global_excludes)
    out = []
    for rel in _WALK_CACHE[key]:
        if _matches(rel, globs) and not _matches(rel, excludes):
            out.append(root / rel)
    return out


def _workspace_dependencies(root: Path, manifest: Path, doc: dict) -> dict:
    """[workspace.dependencies] of the nearest enclosing workspace manifest —
    the audited manifest itself first (a non-virtual root carries [package] and
    [workspace] in one file), then its ancestors up to root; {} when there is
    none. A `workspace = true` dependency inherits its identity from here,
    including a `package = ...` rename."""
    ws = doc.get("workspace")
    if isinstance(ws, dict):
        deps = ws.get("dependencies")
        return deps if isinstance(deps, dict) else {}
    # An explicit [package].workspace = "<dir>" selects a root that need not be an
    # ancestor; Cargo resolves it relative to the package's directory.
    pkg = doc.get("package")
    sel = pkg.get("workspace") if isinstance(pkg, dict) else None
    if isinstance(sel, str) and sel:
        cand = (manifest.parent / sel / "Cargo.toml").resolve()
        if not cand.is_relative_to(root.resolve()):
            raise AuditError(f"{manifest.relative_to(root).as_posix()}: [package].workspace = {sel!r} selects a root outside the repository ({cand}); refusing to read it")
        rel = cand.relative_to(root.resolve()).as_posix()
        try:
            wdoc = tomllib.loads(cand.read_text(encoding="utf-8"))
        except (tomllib.TOMLDecodeError, UnicodeDecodeError) as e:
            raise AuditError(f"{rel}: selected workspace root is not a valid Cargo manifest: {e}") from e
        except OSError as e:
            raise AuditError(f"cannot read selected workspace root {rel}: {e.strerror or e}") from e
        wws = wdoc.get("workspace")
        deps = wws.get("dependencies") if isinstance(wws, dict) else None
        return deps if isinstance(deps, dict) else {}
    d = manifest.parent
    while True:
        cand = d / "Cargo.toml"
        if cand != manifest and cand.is_file():
            try:
                doc = tomllib.loads(cand.read_text(encoding="utf-8"))
            except (tomllib.TOMLDecodeError, UnicodeDecodeError) as e:
                raise AuditError(f"{cand.relative_to(root).as_posix()}: not a valid Cargo manifest: {e}") from e
            except OSError as e:
                raise AuditError(f"cannot read {cand.relative_to(root).as_posix()}: {e.strerror or e}") from e
            ws = doc.get("workspace")
            if isinstance(ws, dict):
                deps = ws.get("dependencies")
                return deps if isinstance(deps, dict) else {}
        if d == root or d.parent == d:
            return {}
        d = d.parent


def _cargo_dependency_sites(root: Path, manifest: Path, rel: str, text: str, crate: str) -> list[tuple[str, int | None, str]]:
    """Every dependency whose RESOLVED package is <crate> in one parsed Cargo
    manifest: the key names the package unless `package = ...` renames it;
    `workspace = true` inherits both from [workspace.dependencies]. Covers
    [dependencies], [dev-dependencies], [build-dependencies] and target-specific
    tables. A parsed manifest has no comments and no quoting, so none of those
    matter; `mu-core = { package = "other" }` is a dependency on other, not a
    site."""
    try:
        doc = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise AuditError(f"{rel}: not a valid Cargo manifest: {e}")
    # Cargo's dependency tables, including the deprecated underscore aliases it
    # still accepts before edition 2024. A dependency under either spelling is
    # live, so both are enumerated (an entry in both counts twice — the strict
    # direction for a ratchet).
    table_names = ("dependencies", "dev-dependencies", "build-dependencies", "dev_dependencies", "build_dependencies")
    tables: list[tuple[str, dict]] = []
    for name in table_names:
        if isinstance(doc.get(name), dict):
            tables.append((name, doc[name]))
    for tgt, spec in (doc.get("target") or {}).items() if isinstance(doc.get("target"), dict) else []:
        if isinstance(spec, dict):
            for name in table_names:
                if isinstance(spec.get(name), dict):
                    tables.append((f"target.{tgt}.{name}", spec[name]))
    ws_deps: dict | None = None
    out = []
    for table, deps in tables:
        for key, spec in deps.items():
            package = spec.get("package") if isinstance(spec, dict) else None
            inherited = isinstance(spec, dict) and spec.get("workspace") is True
            if inherited:
                # The workspace entry is authoritative for an inherited dependency;
                # a member-local `package` next to `workspace = true` is not valid
                # Cargo and must not be able to mask the inherited identity.
                if ws_deps is None:
                    ws_deps = _workspace_dependencies(root, manifest, doc)
                ws_spec = ws_deps.get(key)
                package = ws_spec.get("package") if isinstance(ws_spec, dict) else None
            resolved = package if package is not None else key
            if resolved == crate:
                how = key if package is None else f"{key} = {{ package = {package!r}{', workspace = true' if inherited else ''} }}"
                out.append((rel, None, f"[{table}] {how}"))
    return out


_CFG_TEST_ATTR = re.compile(r"^\s*#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
_CHAR_LIT = re.compile(r"'(\\u\{[0-9a-fA-F_]+\}|\\.|[^'\\])'")


def _lex_lines(text: str) -> list[dict]:
    """One pass over a Rust file, carrying lexical state across lines: block
    comments (nested, as Rust nests them), string literals (which may span
    lines) and raw strings (r"…", r#"…"#, with a `b` or `c` prefix too). For each
    line: `code` — the line's code characters with every literal collapsed to
    its opening quote and every comment removed, each paired with its column
    in the original line — and whether the line began in code (an attribute
    inside a comment or string is not one)."""
    infos: list[dict] = []
    mode = "code"          # code | block | str | raw
    block_depth = 0
    raw_close = ""
    for line in text.splitlines():
        code: list[tuple[int, str]] = []
        began_in_code = mode == "code"
        i = 0
        n = len(line)
        while i < n:
            c = line[i]
            if mode == "block":
                if line.startswith("/*", i):
                    block_depth += 1
                    i += 2
                elif line.startswith("*/", i):
                    block_depth -= 1
                    i += 2
                    if block_depth == 0:
                        mode = "code"
                else:
                    i += 1
                continue
            if mode == "str":
                if c == "\\":
                    i += 2
                    continue
                if c == '"':
                    mode = "code"
                i += 1
                continue
            if mode == "raw":
                j = line.find(raw_close, i)
                if j < 0:
                    i = n
                else:
                    i = j + len(raw_close)
                    mode = "code"
                continue
            # code
            if line.startswith("//", i):
                break
            if line.startswith("/*", i):
                mode = "block"
                block_depth = 1
                i += 2
                continue
            prev_ident = i > 0 and (line[i - 1].isalnum() or line[i - 1] == "_")
            if not prev_ident:
                m = re.match(r"(?:[bc]?r)(#*)\"", line[i:])
                if m:
                    raw_close = '"' + m.group(1)
                    mode = "raw"
                    code.append((i, '"'))
                    i += m.end()
                    continue
                m = re.match(r"[bc]?\"", line[i:])
                if m:
                    mode = "str"
                    code.append((i, '"'))
                    i += m.end()
                    continue
                m = re.match(r"b?" + _CHAR_LIT.pattern, line[i:])
                if m:
                    code.append((i, "'"))
                    i += m.end()
                    continue
            if c == "'":
                # a lifetime or label: the quote alone
                i += 1
                continue
            code.append((i, c))
            i += 1
        infos.append({"code": code, "began_in_code": began_in_code})
    return infos


def _cfg_test_spans(text: str, rel: str) -> list[tuple[int, int, int, int]]:
    """Character ranges `(line, col, end_line, end_col)` (1-based lines,
    inclusive ends) of every `#[cfg(test)]`-attributed item: from the
    attribute through the item's end. The item is whatever code follows the
    attribute — on the attribute's own line or after other attributes,
    comments and blank lines — and it ends, walking code tokens with literals
    and comments already removed, at the `}` that balances its first `{`, or
    for a brace-less item at its `;`, or at the `,` that ends a struct field
    or enum variant (a `,` inside the item's own parentheses, brackets or
    generic angle brackets is not the end — a `<` counts as a generic opener
    only when it directly follows an identifier or `>` (`Vec<`, `fn f<`,
    `impl<`, `Option<Vec<`), never as an operator (`1 < 2`, `a << b`, `x <=`),
    so a comparison cannot hold a span open; an unmatched generic `<` is
    dropped at the item's `;`; a `,` inside a `where` clause is not the end
    either), or just before the `}` that closes the
    enclosing scope. A second item on the same line as the end is outside
    the range, which is why ranges are columns and not lines. Nested
    modules fall inside the outer span by construction. The attribute is
    recognised only in code, and only as `#[cfg(test)]` itself; `cfg(all(test,
    ...))` and friends are left to `exclude`. An item that never ends before
    the end of the file is an AuditError: the audit will not guess where test
    code ends."""
    infos = _lex_lines(text)
    tokens: list[tuple[int, int, str]] = []   # (line 1-based, col, char)
    line_starts: list[int] = []               # index into tokens where each line begins
    for ln, info in enumerate(infos, 1):
        line_starts.append(len(tokens))
        tokens.extend((ln, col, ch) for col, ch in info["code"])
    spans: list[tuple[int, int, int, int]] = []
    ln = 0
    while ln < len(infos):
        info = infos[ln]
        text_of_line = "".join(ch for _, ch in info["code"])
        m = _CFG_TEST_ATTR.match(text_of_line) if info["began_in_code"] else None
        if not m:
            ln += 1
            continue
        first = line_starts[ln] + text_of_line.index("#")
        t = line_starts[ln] + m.end()            # first token after the attribute
        depth = paren = angle = 0
        opened = False
        in_where = False       # a `where` clause's commas do not end the item
        word = ""              # the identifier being read, for `where`
        end = -1
        prev = t - 1
        while t < len(tokens):
            _, _, ch = tokens[t]
            if ch == "{":
                depth += 1
                opened = True
            elif ch == "}":
                if depth == 0:
                    end = prev      # the enclosing scope closed: the item ended before it
                    break
                depth -= 1
                if opened and depth == 0:
                    end = t
                    break
            elif not opened:
                if ch.isalnum() or ch == "_":
                    word += ch
                else:
                    if word == "where":
                        in_where = True
                    word = ""
                if ch == "(" or ch == "[":
                    paren += 1
                elif ch == ")" or ch == "]":
                    paren -= 1
                elif ch == "<":
                    # a generic opener directly follows an identifier or a `>`
                    # on the same line; an operator does not (`1 < 2`, `a << b`)
                    p_ln, p_col, p_ch = tokens[t - 1] if t > 0 else (0, -1, " ")
                    cur_ln, cur_col, _ = tokens[t]
                    adjacent = p_ln == cur_ln and p_col == cur_col - 1
                    if adjacent and (p_ch.isalnum() or p_ch in "_>"):
                        angle += 1
                elif ch == ">" and angle > 0 and not (t > 0 and tokens[t - 1][2] == "-"):
                    angle -= 1
                elif ch == ";" and paren <= 0:
                    end = t
                    break
                elif ch == "," and paren <= 0 and angle <= 0 and not in_where:
                    end = t
                    break
            prev = t
            t += 1
        if end < 0:
            raise AuditError(
                f"{rel}:{ln + 1}: the #[cfg(test)] item never ends before the end of the file; "
                f"the audit will not guess where test code ends (fix the file, or exclude it)"
            )
        sl, sc, _ = tokens[first]
        el, ec, _ = tokens[end]
        spans.append((sl, sc, el, ec))
        # Resume on the line after the item's end (`el` is 1-based, `ln` is
        # 0-based, so `el` is already the next 0-based index). An attribute
        # that follows the end on the same line is not recognised: the
        # attribute regex anchors at the line's first code.
        ln = el
    return spans


def _in_cfg_test(spans: list[tuple[int, int, int, int]], line: int, col: int) -> bool:
    return any((sl, sc) <= (line, col) <= (el, ec) for sl, sc, el, ec in spans)


def _sites(root: Path, inv: dict, global_excludes: list[str]) -> tuple[list[tuple[str, int | None, str]], int]:
    """(every site, sites skipped as `#[cfg(test)]` regions). The skipped count
    is zero unless the invariant sets `skip_cfg_test`."""
    files = _files(root, inv["paths"], list(inv.get("exclude", [])), global_excludes)
    kind = inv.get("kind", "content")
    if kind == "path":
        return [(f.relative_to(root).as_posix(), None, "") for f in files], 0
    if kind == "cargo-dependency":
        out = []
        for f in files:
            rel = f.relative_to(root).as_posix()
            try:
                text = f.read_text(encoding="utf-8")   # strict: a manifest that is not UTF-8 is not a manifest
            except OSError as e:
                raise AuditError(f"cannot read scoped file {rel}: {e.strerror or e}") from e
            except UnicodeDecodeError as e:
                raise AuditError(f"{rel}: not valid UTF-8 ({e.reason} at byte {e.start})") from e
            out.extend(_cargo_dependency_sites(root, f, rel, text, inv["crate"]))
        return out, 0
    if kind != "content":
        raise AuditError(f"invariant {inv['id']!r}: unknown kind {kind!r} (content|path)")
    flags = re.IGNORECASE if inv.get("ignore_case") else 0
    rx = re.compile(inv["pattern"], flags)
    skip_tests = bool(inv.get("skip_cfg_test", False))
    out = []
    skipped = 0
    for f in files:
        rel = f.relative_to(root).as_posix()
        try:
            text = f.read_text(encoding="utf-8", errors="replace")
        except OSError as e:
            raise AuditError(f"cannot read scoped file {rel}: {e.strerror or e}") from e
        spans = _cfg_test_spans(text, rel) if skip_tests else []
        for n, line in enumerate(text.splitlines(), 1):
            matches = [m.start() for m in rx.finditer(line)]
            if not matches:
                continue
            # one site per line, as before; the line is a site if ANY match on
            # it lies outside every test span (a test item and a product item
            # sharing a line cannot hide the product match)
            if spans and all(_in_cfg_test(spans, n, col) for col in matches):
                skipped += 1
            else:
                out.append((rel, n, line.strip()[:160]))
    return out, skipped


def _parse_shapes(text: str, what: str) -> dict:
    try:
        return tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise AuditError(f"{what}: not valid TOML: {e}") from e


def _require_str_list(value, what: str) -> None:
    """TOML makes `paths = "src"` valid; iterating that string as globs audits
    nothing and reports success. Only a list of non-empty strings is a scope."""
    if not isinstance(value, list) or not all(isinstance(x, str) and x for x in value):
        raise AuditError(f"{what} must be a list of non-empty strings (got {type(value).__name__})")


def _validate(invs: list[dict], what: str) -> None:
    seen: set[str] = set()
    for inv in invs:
        if not isinstance(inv, dict):
            raise AuditError(f"{what}: every [[invariant]] must be a table")
        for key in ("id", "rule", "paths"):
            if key not in inv:
                raise AuditError(f"{what}: an [[invariant]] is missing required key {key!r}")
        for key in ("id", "rule"):
            if not isinstance(inv[key], str) or not inv[key]:
                raise AuditError(f"{what}: invariant {key} must be a non-empty string (got {type(inv[key]).__name__})")
        if "pattern" in inv and not isinstance(inv["pattern"], str):
            raise AuditError(f"{what}: invariant {inv['id']!r}: pattern must be a string (got {type(inv['pattern']).__name__})")
        if "kind" in inv and not isinstance(inv["kind"], str):
            raise AuditError(f"{what}: invariant {inv['id']!r}: kind must be a string")
        if "ignore_case" in inv and not isinstance(inv["ignore_case"], bool):
            raise AuditError(f"{what}: invariant {inv['id']!r}: ignore_case must be true/false")
        if "skip_cfg_test" in inv:
            if not isinstance(inv["skip_cfg_test"], bool):
                raise AuditError(f"{what}: invariant {inv['id']!r}: skip_cfg_test must be true/false")
            if inv.get("kind", "content") != "content":
                raise AuditError(f"{what}: invariant {inv['id']!r}: skip_cfg_test applies to kind=content only")
        if inv["id"] in seen:
            raise AuditError(f"{what}: duplicate invariant id {inv['id']!r}")
        seen.add(inv["id"])
        for key in ("paths", "exclude"):
            if key in inv:
                _require_str_list(inv[key], f"{what}: invariant {inv['id']!r}: {key}")
        if not inv["paths"]:
            raise AuditError(f"{what}: invariant {inv['id']!r}: paths must name at least one glob")
        for g in list(inv["paths"]) + list(inv.get("exclude", [])):
            _glob_to_re(g)
        kind = inv.get("kind", "content")
        if kind not in ("content", "path", "cargo-dependency"):
            raise AuditError(f"{what}: invariant {inv['id']!r}: unknown kind {kind!r} (content|path|cargo-dependency)")
        if kind == "cargo-dependency":
            if not isinstance(inv.get("crate"), str) or not inv["crate"]:
                raise AuditError(f"{what}: invariant {inv['id']!r} is kind=cargo-dependency but has no crate")
        if kind == "content":
            if "pattern" not in inv:
                raise AuditError(f"{what}: invariant {inv['id']!r} is kind=content but has no pattern")
            try:
                re.compile(inv["pattern"], re.IGNORECASE if inv.get("ignore_case") else 0)
            except re.error as e:
                raise AuditError(f"{what}: invariant {inv['id']!r}: bad regex: {e}") from e
        if not isinstance(inv.get("baseline", 0), int) or isinstance(inv.get("baseline", 0), bool):
            raise AuditError(f"{what}: invariant {inv['id']!r}: baseline must be an integer")
        if not isinstance(inv.get("retired", False), bool):
            raise AuditError(f"{what}: invariant {inv['id']!r}: retired must be true/false")


def _shapes_from(cfg: dict, what: str) -> tuple[list[str], list[dict]]:
    """The one structural check for a shapes document, used for the checkout's
    file and BASE's alike: the containers must be the right kind before the
    entries are validated, or a malformed file reads as an empty one."""
    settings = cfg.get("settings", {})
    if not isinstance(settings, dict):
        raise AuditError(f"{what}: [settings] must be a table")
    excludes = settings.get("exclude", DEFAULT_EXCLUDES)
    _require_str_list(excludes, f"{what}: settings.exclude")
    invs = cfg.get("invariant", [])
    if not isinstance(invs, list):
        raise AuditError(f"{what}: invariant must be an array of tables ([[invariant]]), got {type(invs).__name__}")
    _validate(invs, what)
    return list(excludes), invs


def load_config(path: Path) -> tuple[list[str], list[dict]]:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as e:
        raise AuditError(f"cannot read {path}: {e.strerror or e}") from e
    except UnicodeDecodeError as e:
        raise AuditError(f"{path}: not valid UTF-8 ({e.reason} at byte {e.start})") from e
    excludes, invs = _shapes_from(_parse_shapes(text, str(path)), str(path))
    return excludes, invs


def _file_at_rev(root: Path, rev: str, rel: str) -> str | None:
    """Contents of <rel> at <rev>; None only when the revision resolves and the
    file is verifiably absent there. Any other failure is an AuditError: a gate
    that cannot see BASE must not fall back to the baselines it is checking."""

    def run(cmd: list[str]) -> subprocess.CompletedProcess | None:
        try:
            return subprocess.run(cmd, cwd=root, capture_output=True, text=True, timeout=60)
        except FileNotFoundError:
            return None
        except (OSError, subprocess.TimeoutExpired) as e:
            raise AuditError(f"{' '.join(cmd[:2])}: {e}") from e

    absent_markers = ("No such path", "does not exist", "exists on disk, but not in", "Path not found")
    # jj first: workspaces have no .git of their own.
    r = run(["jj", "log", "-r", rev, "--no-graph", "-T", "commit_id"])
    if r is not None and r.returncode == 0 and r.stdout.strip():
        r2 = run(["jj", "file", "show", "-r", rev, "--", rel])
        if r2 is not None and r2.returncode == 0:
            return r2.stdout
        if r2 is not None and any(m in r2.stderr for m in absent_markers):
            return None
        raise AuditError(f"jj could not read {rel} at {rev}: {(r2.stderr if r2 else '').strip()[:200]}")
    r = run(["git", "rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}"])
    if r is not None and r.returncode == 0:
        r2 = run(["git", "show", f"{rev}:{rel}"])
        if r2 is not None and r2.returncode == 0:
            return r2.stdout
        if r2 is not None and any(m in r2.stderr for m in absent_markers):
            return None
        raise AuditError(f"git could not read {rel} at {rev}: {(r2.stderr if r2 else '').strip()[:200]}")
    raise AuditError(f"cannot resolve BASE revision {rev!r} with jj or git; pass --base-config or --no-base deliberately")


def load_base(root: Path, config: Path, base_rev: str | None, base_config: Path | None) -> tuple[dict | None, str]:
    """({"excludes": [...], "invariants": {id: entry}} at BASE, description of
    where it came from). The whole entry is kept: the ratchet pins the shape
    that is counted with, not just the number.
    BASE is read at the config's repository-relative path, so a shapes file under
    a subdirectory pins to the same file at BASE, not to a root-level namesake."""
    text = None
    src = ""
    config_name = config.name
    if base_rev and base_config is None:
        try:
            config_name = config.resolve().relative_to(root.resolve()).as_posix()
        except ValueError as e:
            raise AuditError(f"--config {config} is outside the repository root {root}; pass --base-config or --no-base") from e
    if base_config is not None:
        try:
            text = base_config.read_text(encoding="utf-8")
        except OSError as e:
            raise AuditError(f"cannot read BASE shapes file {base_config}: {e.strerror or e}") from e
        except UnicodeDecodeError as e:
            raise AuditError(f"BASE shapes file {base_config}: not valid UTF-8 ({e.reason} at byte {e.start})") from e
        src = f"baselines pinned to {base_config}"
    elif base_rev:
        text = _file_at_rev(root, base_rev, config_name)
        src = f"baselines pinned to BASE {base_rev}"
    if text is None:
        why = "no BASE requested" if not base_rev and base_config is None else f"no {config_name} at BASE {base_rev}"
        return None, f"{why}: using the checkout's own baselines (first commit / push to main)"
    excludes, invs = _shapes_from(_parse_shapes(text, "BASE shapes file"), "BASE shapes file")
    return {"excludes": excludes, "invariants": {inv["id"]: inv for inv in invs}}, src


_SHAPE_KEYS = ("kind", "paths", "exclude", "pattern", "ignore_case", "crate", "skip_cfg_test")


def _shape(inv: dict) -> tuple:
    return tuple((k, tuple(inv[k]) if isinstance(inv.get(k), list) else inv.get(k)) for k in _SHAPE_KEYS)


def audit(root: Path, config: Path, report: bool, base_rev: str | None, base_config: Path | None) -> int:
    excludes, invs = load_config(config)
    base, base_src = load_base(root, config, base_rev, base_config)
    print(base_src)
    failures = 0
    head_ids = {inv["id"] for inv in invs}
    base_invs: dict[str, dict] = base["invariants"] if base is not None else {}
    base_excludes: list[str] = base["excludes"] if base is not None else []
    for bid, binv in base_invs.items():
        if bid not in head_ids and not binv.get("retired", False):
            print(f"{bid}: VIOLATION — present at BASE but missing here; keep the entry with `retired = true` "
                  f"(say why in `rule`) instead of deleting it")
            failures += 1
    for inv in invs:
        head_b = int(inv.get("baseline", 0))
        retired = bool(inv.get("retired", False))
        if retired:
            print(f"{inv['id']}: retired — not counted ({inv['rule'][:120]})")
            continue
        notes = []
        ceiling = head_b
        binv = base_invs.get(inv["id"])
        pinned = binv is not None and not binv.get("retired", False)
        if base is not None and binv is None:
            notes.append(f"new invariant (not at BASE): baseline initialised at {head_b}")
        elif base is not None and binv.get("retired", False):
            notes.append(f"re-activated (retired at BASE): baseline initialised at {head_b}")
        sites, skipped = _sites(root, inv, excludes)
        n = len(sites)
        if pinned:
            base_b = int(binv.get("baseline", 0))
            if _shape(binv) == _shape(inv) and base_excludes == excludes:
                # same shape: the number may only come down
                if head_b > base_b:
                    notes.append(f"VIOLATION: baseline raised from {base_b} (BASE) to {head_b}; baselines only go down")
                    failures += 1
                ceiling = min(head_b, base_b)
            else:
                # the shape changed: BASE's shape is still enforced at BASE's ceiling
                # against THIS checkout, so narrowing cannot hide a new site; the new
                # shape's own count must simply be recorded exactly (checked below).
                base_sites, _ = _sites(root, binv, base_excludes)
                n_base_shape = len(base_sites)
                notes.append(f"shape changed since BASE; BASE's shape still counts {n_base_shape} site(s) here against its ceiling {base_b}")
                if n_base_shape > base_b:
                    notes.append(f"VIOLATION: {n_base_shape} site(s) under BASE's shape exceed BASE's baseline {base_b}")
                    for rel, line, text in base_sites:
                        loc = f"{rel}:{line}" if line is not None else rel
                        notes.append(f"  [BASE shape] {loc}" + (f": {text}" if text else ""))
                    failures += 1
                ceiling = head_b
        if n > ceiling:
            status = "VIOLATION"
            failures += 1
        elif n < head_b:
            status = "stale baseline"   # counted in both modes: --report previews the gate
            failures += 1
        else:
            status = "ok"
        skipped_note = f" ({skipped} in #[cfg(test)] skipped)" if inv.get("skip_cfg_test") else ""
        print(f"{inv['id']}: {n} site(s){skipped_note}, baseline {head_b} — {status}")
        for note in notes:
            print(f"  {note}")
        if status != "ok" or report or any(x.startswith("VIOLATION") for x in notes):
            print(f"  rule: {inv['rule']}")
            for rel, line, text in sites:
                loc = f"{rel}:{line}" if line is not None else rel
                print(f"  {loc}" + (f": {text}" if text else ""))
        if n < head_b:
            print(f"  hint: sites were removed — lower `baseline` for {inv['id']!r} to {n} in {config.name}"
                  + ("" if report else " (a stale baseline fails the gate)"))
    if failures and not report:
        print(f"\ninvariant-audit: {failures} failure(s) — fix every listed site, lower a stale baseline, "
              f"or argue the shape in {config.name}; baselines are only ever lowered.")
        return 1
    print(f"\ninvariant-audit: {len(invs)} invariant(s) checked, {failures} would fail"
          + (" (report mode: not failing)" if report and failures else ""))
    return 0


# --- self-test -------------------------------------------------------------

def _run(root: Path, *args: str) -> tuple[int, str]:
    base = () if any(a.startswith("--base") for a in args) else ("--no-base",)
    proc = subprocess.run(
        [sys.executable, __file__, "--root", str(root), "--config", str(root / ".invariants.toml"), *base, *args],
        capture_output=True, text=True,
    )
    return proc.returncode, proc.stdout + proc.stderr


def self_test() -> int:
    passed = failed = 0

    def check(name: str, cond: bool, out: str = "") -> None:
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"  ok   {name}")
        else:
            failed += 1
            print(f"  FAIL {name}\n{out}")

    with tempfile.TemporaryDirectory(prefix="invariant-audit-") as td:
        root = Path(td)
        (root / "src").mkdir()
        (root / "docs").mkdir()
        (root / "src" / "a.txt").write_text("keep\nFILE_IPC here\n")
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-file-ipc"\nrule = "live data crosses as native types; files are for capture and replay only"\n'
            'kind = "content"\npaths = ["src/**/*.txt"]\npattern = "FILE_IPC"\nbaseline = 1\n\n'
            '[[invariant]]\nid = "no-design-docs-here"\nrule = "design docs live in specs/"\n'
            'kind = "path"\npaths = ["docs/*.md"]\nexclude = ["**/README.md"]\nbaseline = 0\n'
        )
        rc, out = _run(root)
        check("at baseline exits 0", rc == 0 and "no-file-ipc: 1 site(s), baseline 1 — ok" in out, out)

        (root / "src" / "b.txt").write_text("FILE_IPC again\n")
        rc, out = _run(root)
        check("one added site exits 1", rc == 1, out)
        check("violation names the rule", "live data crosses as native types" in out, out)
        check("violation lists the new site", "src/b.txt:1" in out, out)
        check("violation lists the old site too (every site, not just the diff)", "src/a.txt:2" in out, out)

        rc, out = _run(root, "--report")
        check("--report never fails", rc == 0 and "src/b.txt:1" in out, out)

        (root / "src" / "a.txt").unlink()
        (root / "src" / "b.txt").unlink()
        rc, out = _run(root)
        check("fewer sites than the baseline fails the gate with a lower-baseline hint", rc == 1 and "lower `baseline` for 'no-file-ipc' to 0" in out, out)
        rc, out = _run(root, "--report")
        check("--report exits 0 on a stale baseline but counts it as a would-fail", rc == 0 and "lower `baseline`" in out and "1 would fail" in out, out)

        (root / "src" / "a.txt").write_text("FILE_IPC here\n")   # back at baseline 1
        (root / "docs" / "README.md").write_text("fine\n")
        rc, out = _run(root)
        check("path kind: excluded file is not a site", rc == 0, out)
        (root / "docs" / "design.md").write_text("not fine\n")
        rc, out = _run(root)
        check("path kind: a matching file is a violation at baseline 0", rc == 1 and "docs/design.md" in out, out)

        (root / "src" / "a.txt").unlink()
        (root / "target").mkdir()
        (root / "target" / "gen.txt").write_text("FILE_IPC generated\n")
        (root / "src" / "c.txt").write_text("FILE_IPC\n")
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-file-ipc"\nrule = "r"\nkind = "content"\npaths = ["**/*.txt"]\npattern = "FILE_IPC"\nbaseline = 1\n'
        )
        rc, out = _run(root)
        check("default excludes skip target/", rc == 0 and "target/gen.txt" not in out, out)
        # fresh state for the last cases: one site, one invariant, baseline 1
        for f in (root / "src").glob("*.txt"):
            f.unlink()
        (root / "docs" / "design.md").unlink()
        (root / "target" / "gen.txt").unlink()
        # base pinning: BASE says 1; the checkout says 2 and has 2 sites
        (root / "src" / "a.txt").write_text("FILE_IPC\n")
        (root / "src" / "b.txt").write_text("FILE_IPC\n")
        basecfg = root / "base.toml"
        basecfg.write_text('[[invariant]]\nid = "no-file-ipc"\nrule = "r"\nkind = "content"\npaths = ["src/**/*.txt"]\npattern = "FILE_IPC"\nbaseline = 1\n')
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-file-ipc"\nrule = "r"\nkind = "content"\npaths = ["src/**/*.txt"]\npattern = "FILE_IPC"\nbaseline = 2\n\n'
            '[[invariant]]\nid = "brand-new"\nrule = "r2"\nkind = "path"\npaths = ["docs/*.md"]\nexclude = ["**/README.md"]\nbaseline = 1\n'
        )
        rc, out = _run(root, "--base-config", str(basecfg))
        check("a raised baseline is a violation against BASE", rc == 1 and "baseline raised from 1 (BASE) to 2" in out, out)
        check("an invariant absent from BASE initialises at the checkout's value", "new invariant (not at BASE): baseline initialised at 1" in out, out)
        (root / "src" / "b.txt").unlink()
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-file-ipc"\nrule = "r"\nkind = "content"\npaths = ["src/**/*.txt"]\npattern = "FILE_IPC"\nbaseline = 1\n'
        )
        rc, out = _run(root, "--base-config", str(basecfg))
        check("at BASE's baseline with the same count passes", rc == 0 and "no-file-ipc: 1 site(s), baseline 1 — ok" in out, out)

        # skip_cfg_test (mu-hg9eo): matches inside #[cfg(test)] items are not
        # sites, nested modules included; the skipped count is reported; the
        # flag is part of the pinned shape, so BASE's un-flagged shape still
        # counts at its own ceiling while the flagged one records its number.
        (root / "src" / "lib.rs").write_text(
            'pub fn work() {\n'
            '    std::thread::sleep(d); // product site 1\n'
            '    let s = "}"; // a brace in a string must not end a span\n'
            '}\n'
            '#[cfg(test)]\n'
            'use std::thread::sleep; // attribute on a brace-less item: its own lines only\n'
            'pub fn more() { std::thread::sleep(d) } // product site 2\n'
            '#[cfg(test)]\n'
            '#[allow(dead_code)]\n'
            'mod tests {\n'
            '    use super::*;\n'
            '    fn a() { std::thread::sleep(d) } // test site\n'
            '    mod inner {\n'
            '        fn b() { let c = \'{\'; std::thread::sleep(d) } // nested test site, char literal brace\n'
            '    }\n'
            '    /* a comment with } inside */\n'
            '    fn c() { std::thread::sleep(d) } // test site after the comment\n'
            '}\n'
            '#[cfg(test)]\n'
            'fn helper() { std::thread::sleep(d) } // cfg(test) fn: test site\n'
            'pub fn last() { std::thread::sleep(d) } // product site 3, after the spans\n'
        )
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-sleep"\nrule = "no new sleep sites"\nkind = "content"\n'
            'paths = ["src/**/*.rs"]\npattern = "thread::sleep\\\\("\nbaseline = 7\n'
        )
        rc, out = _run(root, "--no-base")
        check("without skip_cfg_test every match counts (7: 3 product, 4 in test regions)",
              rc == 0 and "no-sleep: 7 site(s), baseline 7 — ok" in out, out)
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-sleep"\nrule = "no new sleep sites"\nkind = "content"\n'
            'paths = ["src/**/*.rs"]\npattern = "thread::sleep\\\\("\nskip_cfg_test = true\nbaseline = 3\n'
        )
        rc, out = _run(root, "--no-base")
        check("skip_cfg_test drops the test-region matches and says how many",
              rc == 0 and "no-sleep: 3 site(s) (4 in #[cfg(test)] skipped), baseline 3 — ok" in out, out)
        rc, out = _run(root, "--report", "--no-base")
        check("the skipped sites are not listed as sites", "src/lib.rs:2" in out and "src/lib.rs:7" in out and "src/lib.rs:21" in out
              and "src/lib.rs:12" not in out and "src/lib.rs:14" not in out and "src/lib.rs:17" not in out and "src/lib.rs:20" not in out, out)
        basecfg2 = root / "base2.toml"
        basecfg2.write_text(
            '[[invariant]]\nid = "no-sleep"\nrule = "r"\nkind = "content"\n'
            'paths = ["src/**/*.rs"]\npattern = "thread::sleep\\\\("\nbaseline = 7\n'
        )
        rc, out = _run(root, "--base-config", str(basecfg2))
        check("adding skip_cfg_test is a shape change: BASE's shape still counts 7 against its ceiling 7, the new shape records 3",
              rc == 0 and "shape changed since BASE; BASE's shape still counts 7 site(s) here against its ceiling 7" in out
              and "no-sleep: 3 site(s) (4 in #[cfg(test)] skipped), baseline 3 — ok" in out, out)
        (root / "src" / "lib.rs").write_text((root / "src" / "lib.rs").read_text() + 'pub fn added() { std::thread::sleep(d) }\n')
        rc, out = _run(root, "--base-config", str(basecfg2))
        check("a new product site still fails under the flagged shape", rc == 1 and "no-sleep: 4 site(s) (4 in #[cfg(test)] skipped), baseline 3 — VIOLATION" in out, out)
        # the scanner's edges (board round 1): an item on the attribute's own
        # line, attributes inside comments and strings, literals that span
        # lines or carry braces, and a span that never closes.
        (root / "src" / "lib.rs").write_text(
            '#[cfg(test)] fn helper() { std::thread::sleep(d) }\n'
            'fn production() { std::thread::sleep(d) } // product: the line after a same-line item\n'
            '/*\n'
            '#[cfg(test)]\n'
            '*/\n'
            'fn p2() { std::thread::sleep(d) } // product: the attribute above is in a comment\n'
            'const S: &str = r#"\n'
            '#[cfg(test)]\n'
            '}"#;\n'
            'fn p3() { std::thread::sleep(d) } // product: attribute and brace in a raw string\n'
            '#[cfg(test)]\n'
            'mod tests {\n'
            '    const T: &str = r"multi\n'
            '}\n'
            'line"; // the } above is inside the raw string\n'
            '    const B: &[u8] = b"}"; let c = b\'{\'; let u = \'\\u{7D}\'; /* nested /* } */ */\n'
            '    fn t() { std::thread::sleep(d) } // test\n'
            '}\n'
            '#[cfg(test)] mod tests_out;\n'
            'fn p4() { std::thread::sleep(d) } // product after a brace-less same-line item\n'
        )
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-sleep"\nrule = "r"\nkind = "content"\n'
            'paths = ["src/**/*.rs"]\npattern = "thread::sleep\\\\("\nskip_cfg_test = true\nbaseline = 4\n'
        )
        rc, out = _run(root, "--report", "--no-base")
        check("same-line items, commented/quoted attributes and multi-line or byte literals do not swallow product sites",
              rc == 0 and "no-sleep: 4 site(s) (2 in #[cfg(test)] skipped), baseline 4 — ok" in out
              and "src/lib.rs:2" in out and "src/lib.rs:6" in out and "src/lib.rs:10" in out and "src/lib.rs:20" in out
              and "src/lib.rs:1" not in out.replace("src/lib.rs:10", "") and "src/lib.rs:17" not in out, out)
        # round 2: a second item on the attribute's line, and attributes on
        # comma-delimited items (struct fields, enum variants) — the span must
        # end at the item, not drift into the next production item.
        (root / "src" / "lib.rs").write_text(
            '#[cfg(test)] fn helper() { std::thread::sleep(d) } fn production() {\n'
            '    std::thread::sleep(d) // product: the item after helper on the same line\n'
            '}\n'
            '#[cfg(test)] fn h2() {} fn p2() { std::thread::sleep(d) } // product, same line as a test item\n'
            'struct S {\n'
            '    #[cfg(test)]\n'
            '    field: Vec<(u8, u8)>,\n'
            '}\n'
            'fn p3() { std::thread::sleep(d) } // product after an attributed field\n'
            'enum E {\n'
            '    #[cfg(test)]\n'
            '    Only\n'
            '}\n'
            'fn p4() { std::thread::sleep(d) } // product after an attributed last variant\n'
            '#[cfg(test)]\n'
            'fn generic<A, B>(a: (A, B)) { std::thread::sleep(d) } // test: commas in the signature\n'
        )
        (root / "src" / "ops.rs").write_text(
            '#[cfg(test)] fn h() { std::thread::sleep(d) } fn p() { std::thread::sleep(d) } // both on one line: a site\n'
            '#[cfg(test)] const FLAG: bool = 1 < 2;\n'
            'fn p2() { std::thread::sleep(d) } // product after a const with a comparison\n'
            '#[cfg(test)] const MASK: u32 = 1 << 4;\n'
            'fn p3() { std::thread::sleep(d) } // product after a const with a shift\n'
            '#[cfg(test)] const V: Vec<(u8, u8)> = Vec::new();\n'
            'fn p4() { std::thread::sleep(d) } // product after a generic-typed const\n'
            '#[cfg(test)]\n'
            'fn w<T>(t: T) where T: Ord, { std::thread::sleep(d) } // test: a where clause comma\n'
            '#[cfg(test)]\n'
            'mod c { const C: &CStr = c"}"; fn t() { std::thread::sleep(d) } } // test: a c-string brace\n'
        )
        rc, out = _run(root, "--report", "--no-base")
        check("a line with a test match and a product match is a site; comparison and shift operators in a brace-less item do not extend the span",
              rc == 0 and "no-sleep: 8 site(s) (4 in #[cfg(test)] skipped), baseline 4 — VIOLATION" in out
              and "src/ops.rs:1" in out and "src/ops.rs:3" in out and "src/ops.rs:5" in out and "src/ops.rs:7" in out
              and "src/ops.rs:9" not in out and "src/ops.rs:11" not in out, out)
        (root / "src" / "ops.rs").unlink()
        rc, out = _run(root, "--report", "--no-base")
        check("a second item on the attribute's line and attributed fields/variants do not extend the span",
              rc == 0 and "no-sleep: 4 site(s) (2 in #[cfg(test)] skipped), baseline 4 — ok" in out
              and "src/lib.rs:2" in out and "src/lib.rs:4" in out and "src/lib.rs:9" in out and "src/lib.rs:14" in out
              and "src/lib.rs:16" not in out, out)
        (root / "src" / "lib.rs").write_text('#[cfg(test)]\nmod tests {\n    fn t() { std::thread::sleep(d) }\n// never closed\n')
        rc, out = _run(root, "--no-base")
        check("a cfg(test) span that never closes fails the audit naming the line, instead of absorbing the file",
              rc == 2 and "src/lib.rs:1: the #[cfg(test)] item never ends" in out, out)
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "x"\nrule = "r"\nkind = "path"\npaths = ["src/*.rs"]\nskip_cfg_test = true\nbaseline = 0\n'
        )
        rc, out = _run(root, "--no-base")
        check("skip_cfg_test on a non-content kind is a shapes-file error (exit 2)", rc == 2 and "applies to kind=content only" in out, out)
        (root / "src" / "lib.rs").unlink()
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-file-ipc"\nrule = "r"\nkind = "content"\npaths = ["src/**/*.txt"]\npattern = "FILE_IPC"\nbaseline = 1\n'
        )
        (root / "src" / "a.txt").write_text("FILE_IPC\n")

        # unreadable scoped file: the gate must not pass
        posix_non_root = hasattr(os, "geteuid") and os.geteuid() != 0
        if posix_non_root:
            (root / "src" / "locked.txt").write_text("FILE_IPC\n")
            (root / "src" / "locked.txt").chmod(0)
            rc, out = _run(root)
            check("an unreadable scoped file fails the audit naming the path", rc == 2 and "src/locked.txt" in out, out)
            (root / "src" / "locked.txt").chmod(0o644)
            (root / "src" / "locked.txt").unlink()
            (root / "src" / "hidden").mkdir()
            (root / "src" / "hidden" / "v.txt").write_text("FILE_IPC\n")
            (root / "src" / "hidden").chmod(0)
            rc, out = _run(root)
            check("an unreadable scoped directory fails the audit naming it", rc == 2 and "src/hidden" in out, out)
            (root / "src" / "hidden").chmod(0o755)
            (root / "src" / "hidden" / "v.txt").unlink()
            (root / "src" / "hidden").rmdir()
        else:
            print("  skip unreadable-file/directory cases (root or non-POSIX)")

        # a directory symlink inside the scope fails closed rather than counting as nothing
        try:
            (root / "src" / "linked").symlink_to(root / "docs", target_is_directory=True)
            rc, out = _run(root)
            check("a scoped directory symlink exits 2 naming it", rc == 2 and "src/linked is a symlink" in out, out)
            (root / "src" / "linked").unlink()
        except (OSError, NotImplementedError):
            print("  skip directory-symlink case (symlinks unsupported here)")

        # relative --config from a different cwd must mean the same file throughout
        proc = subprocess.run([sys.executable, __file__, "--root", root.name, "--config", f"{root.name}/.invariants.toml", "--no-base"],
                              cwd=root.parent, capture_output=True, text=True)
        check("a relative --config from another cwd is read consistently", proc.returncode == 0 and "invariant(s) checked" in proc.stdout, proc.stdout + proc.stderr)

        # error contract: every bad input is exit 2, never a ratchet verdict
        for f in (root / "src").glob("*.txt"):
            f.unlink()
        good = '[[invariant]]\nid = "no-file-ipc"\nrule = "r"\nkind = "content"\npaths = ["src/**/*.txt"]\npattern = "FILE_IPC"\nbaseline = 0\n'
        (root / ".invariants.toml").write_text("this = [is not toml")
        rc, out = _run(root)
        check("invalid TOML exits 2", rc == 2 and "not valid TOML" in out, out)
        (root / ".invariants.toml").write_text(good.replace('pattern = "FILE_IPC"', 'pattern = "("'))
        rc, out = _run(root)
        check("a bad regex exits 2", rc == 2 and "bad regex" in out, out)
        (root / ".invariants.toml").write_text(good.replace("baseline = 0", 'baseline = "0"'))
        rc, out = _run(root)
        check("a non-integer baseline exits 2", rc == 2 and "baseline must be an integer" in out, out)
        (root / ".invariants.toml").write_text(good)
        rc, out = _run(root, "--base", "no-such-revision-xyz")
        check("an unresolvable BASE is exit 2, not a fall-back", rc == 2 and "cannot resolve BASE" in out, out)
        basecfg.write_text("also = [not toml")
        rc, out = _run(root, "--base-config", str(basecfg))
        check("an invalid BASE shapes file exits 2", rc == 2 and "not valid TOML" in out, out)
        (root / ".invariants.toml").write_bytes(b"[[invariant]]\nid = \"\xff\"\n")
        rc, out = _run(root)
        check("a non-UTF-8 shapes file exits 2", rc == 2 and "not valid UTF-8" in out, out)
        (root / ".invariants.toml").write_text(good)
        basecfg.write_bytes(b"[[invariant]]\nid = \"\xff\"\n")
        rc, out = _run(root, "--base-config", str(basecfg))
        check("a non-UTF-8 BASE shapes file exits 2", rc == 2 and "not valid UTF-8" in out, out)
        basecfg.write_text('invariant = ""\n')
        rc, out = _run(root, "--base-config", str(basecfg))
        check("a structurally malformed BASE file exits 2 instead of reading as empty", rc == 2 and "array of tables" in out, out)
        basecfg.write_text('[invariant]\n')
        rc, out = _run(root, "--base-config", str(basecfg))
        check("a BASE [invariant] table (not array) exits 2", rc == 2 and "array of tables" in out, out)

        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = "src/**/*.txt"'))
        rc, out = _run(root)
        check("a scalar-string paths scope exits 2", rc == 2 and "must be a list of non-empty strings" in out, out)
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = []'))
        rc, out = _run(root)
        check("an empty paths scope exits 2", rc == 2 and "at least one glob" in out, out)
        (root / ".invariants.toml").write_text(good)
        for bad_toml, label, needle in (
            (good.replace('id = "no-file-ipc"', 'id = []'), "a non-string id exits 2", "id must be a non-empty string"),
            (good.replace('pattern = "FILE_IPC"', 'pattern = 123'), "a non-string pattern exits 2", "pattern must be a string"),
            (good.replace('rule = "r"', 'rule = 123'), "a non-string rule exits 2", "rule must be a non-empty string"),
            (good.replace('paths = ["src/**/*.txt"]', 'paths = ["src/[ab.txt"]'), "an unterminated character class exits 2", "unterminated character class"),
        ):
            (root / ".invariants.toml").write_text(bad_toml)
            rc, out = _run(root)
            check(label, rc == 2 and needle in out, out)
        (root / "src" / "a.txt").write_text("FILE_IPC\n"); (root / "src" / "b.txt").write_text("FILE_IPC\n"); (root / "src" / "c.txt").write_text("FILE_IPC\n")
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["src/[ab].txt"]').replace("baseline = 0", "baseline = 2"))
        rc, out = _run(root)
        check("a character class selects exactly its members", rc == 0 and "no-file-ipc: 2 site(s), baseline 2 — ok" in out, out)
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["src/[!ab].txt"]').replace("baseline = 0", "baseline = 1"))
        rc, out = _run(root)
        check("a negated character class selects the rest", rc == 0 and "no-file-ipc: 1 site(s), baseline 1 — ok" in out, out)
        for f in (root / "src").glob("*.txt"):
            f.unlink()
        (root / ".invariants.toml").write_text(good)
        # cargo-dependency kind: parsed manifests, every spelling, comments ignored
        (root / "crates").mkdir(exist_ok=True)
        (root / "crates" / "Cargo.toml").write_text(
            '[package]\nname = "p"\n# alias = { package = "mu-core" }  (a comment is not a dependency)\n\n'
            '[dependencies]\nmu-core = { path = "../mu-core" }\nmu-core-extra = "0.1"\n'
            "alias = { package = 'mu-core', path = '../mu-core' }\n\n"
            '[dev-dependencies."mu-core"]\npath = "../mu-core"\n\n'
            "[target.'cfg(unix)'.build-dependencies]\nmu-core = \"0.1\"\n"
        )
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-core-dep"\nrule = "standalone"\nkind = "cargo-dependency"\npaths = ["crates/Cargo.toml"]\ncrate = "mu-core"\nbaseline = 4\n'
        )
        rc, out = _run(root)
        check("cargo-dependency counts bare, renamed, quoted-table and target-table deps (4) and not the comment or mu-core-extra",
              rc == 0 and "no-core-dep: 4 site(s), baseline 4 — ok" in out, out)
        rc, out = _run(root, "--report")
        check("cargo-dependency names the table and the renamed alias", "[dependencies] alias = { package = 'mu-core' }" in out and "[target.cfg(unix).build-dependencies] mu-core" in out, out)
        (root / "crates" / "Cargo.toml").write_text(
            '[package]\nname = "p"\n\n[dev_dependencies]\nmu-core = "0.1"\n\n'
            "[target.'cfg(unix)'.build_dependencies]\ncore_alias = { package = \"mu-core\", path = \"x\" }\n"
        )
        rc, out = _run(root, "--report")
        check("legacy underscore tables (top-level and target-specific) are live dependencies", "[dev_dependencies] mu-core" in out and "[target.cfg(unix).build_dependencies] core_alias" in out and "2 site(s)" in out, out)
        # resolved identity: a renamed key is not a site; a workspace-inherited rename is
        (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates"]\n\n[workspace.dependencies]\ncore_alias = { package = "mu-core", path = "mu-core" }\nplain = "1"\n')
        (root / "crates" / "Cargo.toml").write_text(
            '[package]\nname = "p"\n\n[dependencies]\nmu-core = { package = "other", version = "1" }\n'
            'core_alias = { workspace = true }\nplain = { workspace = true }\n'
        )
        (root / ".invariants.toml").write_text(
            '[[invariant]]\nid = "no-core-dep"\nrule = "standalone"\nkind = "cargo-dependency"\npaths = ["crates/Cargo.toml"]\ncrate = "mu-core"\nbaseline = 1\n'
        )
        rc, out = _run(root, "--report")
        check("a key named mu-core that renames to another package is not a site", "mu-core = { package = 'other'" not in out and "1 site(s)" in out, out)
        check("a workspace-inherited rename onto mu-core is a site", "core_alias = { package = 'mu-core', workspace = true }" in out, out)
        (root / "crates" / "Cargo.toml").write_text(
            '[package]\nname = "p"\n\n[dependencies]\ncore_alias = { workspace = true, package = "other" }\n'
        )
        rc, out = _run(root, "--report")
        check("a member-local package next to workspace = true cannot mask the inherited identity", "core_alias = { package = 'mu-core', workspace = true }" in out and "1 site(s)" in out, out)
        (root / "Cargo.toml").unlink()
        # a non-virtual workspace root inherits from its own [workspace.dependencies]
        (root / "crates" / "Cargo.toml").write_text(
            '[package]\nname = "rootpkg"\n\n[workspace]\nmembers = []\n\n[workspace.dependencies]\ncore_alias = { package = "mu-core", version = "0.1" }\n\n'
            '[dependencies]\ncore_alias = { workspace = true }\n'
        )
        rc, out = _run(root, "--report")
        check("a package that is its own workspace root inherits its own rename onto mu-core", "core_alias = { package = 'mu-core', workspace = true }" in out and "1 site(s)" in out, out)
        # an explicit [package].workspace selector to a non-ancestor root
        (root / "ws").mkdir(exist_ok=True)
        (root / "ws" / "Cargo.toml").write_text('[workspace]\nmembers = []\n\n[workspace.dependencies]\ncore_alias = { package = "mu-core", version = "0.1" }\n')
        (root / "crates" / "Cargo.toml").write_text('[package]\nname = "p"\nworkspace = "../ws"\n\n[dependencies]\ncore_alias = { workspace = true }\n')
        rc, out = _run(root, "--report")
        check("an explicit [package].workspace selector resolves the inherited rename", "core_alias = { package = 'mu-core', workspace = true }" in out and "1 site(s)" in out, out)
        (root / "ws" / "Cargo.toml").unlink(); (root / "ws").rmdir()
        rc, out = _run(root)
        check("a selected workspace root that is missing exits 2", rc == 2 and "selected workspace root" in out, out)
        (root / "crates" / "Cargo.toml").write_text('[package]\nname = "p"\nworkspace = "../../.."\n\n[dependencies]\ncore_alias = { workspace = true }\n')
        rc, out = _run(root)
        check("a workspace selector outside the repository root exits 2", rc == 2 and "outside the repository" in out, out)
        (root / "crates" / "Cargo.toml").write_bytes(b"[dependencies]\nmu-core = \"\xff\xfe\"\n")
        rc, out = _run(root)
        check("a non-UTF-8 manifest exits 2", rc == 2 and "not valid UTF-8" in out, out)
        (root / "crates" / "Cargo.toml").write_text("[dependencies\nbroken = 1\n")
        rc, out = _run(root)
        check("an unparsable manifest exits 2", rc == 2 and "not a valid Cargo manifest" in out, out)
        (root / ".invariants.toml").write_text('[[invariant]]\nid = "x"\nrule = "r"\nkind = "cargo-dependency"\npaths = ["crates/Cargo.toml"]\nbaseline = 0\n')
        rc, out = _run(root)
        check("cargo-dependency without a crate exits 2", rc == 2 and "has no crate" in out, out)
        (root / "crates" / "Cargo.toml").unlink(); (root / "crates").rmdir()
        (root / ".invariants.toml").write_text(good)

        # root-relative globs: a slashless pattern names the root file only
        (root / "src" / "n.txt").write_text("FILE_IPC\n"); (root / "n.txt").write_text("FILE_IPC\n")
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["n.txt"]').replace("baseline = 0", "baseline = 1"))
        rc, out = _run(root)
        check("a slashless path glob matches only the root file", rc == 0 and "no-file-ipc: 1 site(s), baseline 1 — ok" in out, out)
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["**/n.txt"]\nexclude = ["n.txt"]').replace("baseline = 0", "baseline = 1"))
        rc, out = _run(root)
        check("a slashless exclude drops only the root file, not nested namesakes", rc == 0 and "src/n.txt" not in out.split("rule:")[0] and "1 site(s), baseline 1 — ok" in out, out)
        (root / "src" / "n.txt").unlink(); (root / "n.txt").unlink()
        (root / ".invariants.toml").write_text(good)

        # BASE lookup uses the config's repo-relative path (a real git history)
        git_env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1",
                       GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@x", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@x")
        def git(*a):
            return subprocess.run(["git", *a], cwd=root, env=git_env, capture_output=True, text=True)
        if git("--version").returncode == 0:
            (root / "checks").mkdir(exist_ok=True)
            (root / "checks" / "rules.toml").write_text(good)
            (root / "rules.toml").write_text(good.replace("baseline = 0", "baseline = 5"))   # a root-level namesake
            git("init", "-q", "-b", "main"); git("add", "-A"); git("commit", "-q", "-m", "base")
            (root / "src" / "z.txt").write_text("FILE_IPC\n")
            (root / "checks" / "rules.toml").write_text(good.replace("baseline = 0", "baseline = 1"))
            proc = subprocess.run([sys.executable, __file__, "--root", str(root), "--config", str(root / "checks" / "rules.toml"), "--base", "main"],
                                  capture_output=True, text=True, env=git_env)
            out = proc.stdout + proc.stderr
            check("a subdirectory --config pins to the same path at BASE, not a root namesake",
                  proc.returncode == 1 and "baseline raised from 0 (BASE) to 1" in out, out)
            outside = Path(tempfile.gettempdir()) / f"outside-{os.getpid()}.toml"
            outside.write_text(good)
            proc = subprocess.run([sys.executable, __file__, "--root", str(root), "--config", str(outside), "--base", "main"],
                                  capture_output=True, text=True, env=git_env)
            check("a --config outside the root with a BASE rev exits 2", proc.returncode == 2 and "outside the repository root" in (proc.stdout + proc.stderr), proc.stdout + proc.stderr)
            outside.unlink()
            (root / "src" / "z.txt").unlink()
            (root / "rules.toml").unlink()
            (root / "checks" / "rules.toml").unlink()
        else:
            print("  skip git-backed BASE path case (no git)")

        # retirement lifecycle: a BASE invariant may not silently disappear
        basecfg.write_text(good + '\n[[invariant]]\nid = "old-rule"\nrule = "r"\nkind = "path"\npaths = ["docs/*.md"]\nbaseline = 0\n')
        (root / ".invariants.toml").write_text(good)
        rc, out = _run(root, "--base-config", str(basecfg))
        check("deleting a BASE invariant fails naming it", rc == 1 and "old-rule: VIOLATION" in out and "retired = true" in out, out)
        (root / ".invariants.toml").write_text(good + '\n[[invariant]]\nid = "old-rule"\nrule = "retired 2026-09-09: superseded by no-file-ipc"\nkind = "path"\npaths = ["docs/*.md"]\nbaseline = 0\nretired = true\n')
        rc, out = _run(root, "--base-config", str(basecfg))
        check("marking it retired passes and is not counted", rc == 0 and "old-rule: retired" in out, out)
        basecfg.write_text((root / ".invariants.toml").read_text())
        (root / ".invariants.toml").write_text(good)
        rc, out = _run(root, "--base-config", str(basecfg))
        check("once BASE carries the retirement the entry may be deleted", rc == 0, out)

        # shape pinning: narrowing the shape cannot hide a new site
        basecfg.write_text(good)                                   # BASE: src/**/*.txt, baseline 0
        (root / "src" / "new.txt").write_text("FILE_IPC\n")
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["src/nothing/*.txt"]'))
        rc, out = _run(root, "--base-config", str(basecfg))
        check("narrowing the shape while adding a site fails against BASE's shape", rc == 1 and "under BASE's shape exceed BASE's baseline" in out, out)
        check("the BASE-shape violation names the hidden site", "[BASE shape] src/new.txt:1" in out, out)
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["src/**/*.txt"]\nexclude = ["src/new.txt"]'))
        rc, out = _run(root, "--base-config", str(basecfg))
        check("excluding the new site fails against BASE's shape too", rc == 1 and "under BASE's shape exceed BASE's baseline" in out, out)
        (root / "src" / "new.txt").unlink()
        # widening the shape with an exact new baseline is allowed
        (root / "docs" / "w.txt").write_text("FILE_IPC\n")
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["src/**/*.txt", "docs/*.txt"]').replace("baseline = 0", "baseline = 1"))
        rc, out = _run(root, "--base-config", str(basecfg))
        check("widening the shape with an exact new baseline passes", rc == 0 and "shape changed since BASE" in out, out)
        (root / ".invariants.toml").write_text(good.replace('paths = ["src/**/*.txt"]', 'paths = ["src/**/*.txt", "docs/*.txt"]').replace("baseline = 0", "baseline = 3"))
        rc, out = _run(root, "--base-config", str(basecfg))
        check("widening with an inflated baseline is a stale baseline", rc == 1 and "stale baseline" in out, out)
        (root / "docs" / "w.txt").unlink()
        (root / ".invariants.toml").write_text(good)


    print(f"invariant-audit self-test: {passed} passed, {failed} failed")
    return 1 if failed else 0


def main() -> int:
    ap = argparse.ArgumentParser(description="enumerate every site of each violation shape and ratchet the count")
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    ap.add_argument("--config", type=Path, default=None)
    ap.add_argument("--report", action="store_true", help="list every site; never fail")
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--base", default=os.environ.get("MU_INVARIANTS_BASE", "main"),
                    help="revision whose shapes file pins the baselines (default: main)")
    ap.add_argument("--base-config", type=Path, default=None, help="BASE's shapes file, already extracted")
    ap.add_argument("--no-base", action="store_true", help="use the checkout's own baselines")
    args = ap.parse_args()
    if args.self_test:
        return self_test()
    # Resolve every path once, up front: nothing below depends on the cwd
    # (subprocesses get cwd=root explicitly), so a relative --config given from
    # another directory means the same file throughout.
    root = args.root.resolve()
    config = (args.config or (root / ".invariants.toml")).resolve()
    base_config = args.base_config.resolve() if args.base_config is not None else None
    if not config.is_file():
        print(f"invariant-audit: no shapes file at {config}", file=sys.stderr)
        return 2
    try:
        return audit(root, config, args.report, None if args.no_base else args.base, base_config)
    except AuditError as e:
        print(f"invariant-audit: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
