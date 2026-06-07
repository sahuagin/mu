#!/usr/bin/env python3
"""Layer-2 compaction-fidelity probe harness (mu-0fla).

Layer 1 (`crates/mu-core/src/context/compaction/fidelity.rs`) measures
*structurally* what each compaction policy kept. This is Layer 2: it
measures it *behaviorally*. A downstream model is given a compacted rope
as context and asked probe questions whose answers live in specific
spans; an LLM judge scores each answer against a hand-written gold
answer. The fidelity signal is **correctness conditioned on the target
span's fate** under each policy:

  - no-compaction  -> target always kept   -> the control / ceiling.
  - heuristic/judge -> target may be dropped/summarized -> does the model
    still answer (re-derive from surviving context) or fail?

A policy that keeps fidelity high *even when it dropped the target span*
is genuinely lossless-enough; one that fails whenever it drops a span is
paying its compaction in fidelity.

INPUT
  --fixtures   JSON from `cargo run --example compaction-fixtures` — a
               list of RopeFixture, one per policy, for ONE session at a
               fixed target_tokens. The probe set is authored for that
               (session, target), since span survival depends on target.
  --probes     JSON: {"session_id","target_tokens","probes":[
                 {"id","question","gold_answer","target_span_id","note"}]}.
               `target_span_id` is the span whose content answers the
               probe (used to look up the target's fate per policy).

USAGE
  cargo run --release --example compaction-fixtures -p mu-ai -- \
      ~/.local/share/mu/events/<daemon>/<session>.jsonl 3000 > fixtures.json
  python3 scripts/compaction_probe.py \
      --fixtures fixtures.json \
      --probes scripts/compaction_probes/<session>.json \
      --models qwen3.6-det qwen3.6-code \
      --judge-model qwen3.6-det

DETERMINISM (per memory 6679bf86): reproducibility for these qwen3.6
variants comes from a FIXED SEED, not temperature, and `num_ctx` is a
LOAD-TIME Modelfile setting — so by default we do NOT send `num_ctx`
(sending it can force a reload). Run the ollama server with
OLLAMA_NUM_PARALLEL=1 for serial, batch-deterministic decoding.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from typing import Any

DEFAULT_OLLAMA = os.environ.get("OLLAMA_HOST", "http://10.1.1.143:11434")

# Per-call failure modes that must turn into an `unparsed` record rather
# than abort the whole batch (an overnight 60-probe run must not lose all
# results to one mid-batch GPU OOM / model-swap race). Covers transport
# errors, timeouts, bad JSON, and AttributeError from a malformed 200
# response (e.g. `{"message": null}`, where the null is not a missing key).
CALL_ERRORS = (
    urllib.error.URLError,
    http.client.HTTPException,
    TimeoutError,
    OSError,
    json.JSONDecodeError,
    AttributeError,
)

ANSWER_SYSTEM = (
    "You answer questions using ONLY the provided context. The context is a "
    "compacted transcript of a coding session. If the answer is not present "
    "in the context, reply with exactly: NOT IN CONTEXT. Be concise."
)

JUDGE_SYSTEM = (
    "You are a strict grader. Given a QUESTION, a GOLD answer, and a "
    "CANDIDATE answer, decide if the candidate is correct. Output ONLY a "
    'JSON object: {"verdict":"correct|partial|wrong","reason":"<short>"}. '
    '"correct" = captures the gold fact (wording may differ); "partial" = '
    'some of it; "wrong" = incorrect, or NOT IN CONTEXT when the gold has a '
    "real answer."
)


def http_json(url: str, payload: dict[str, Any] | None = None, timeout: int = 900) -> dict[str, Any]:
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


def chat(
    base_url: str,
    model: str,
    system: str,
    user: str,
    *,
    seed: int,
    temperature: float,
    num_ctx: int | None,
    force_json: bool,
    timeout: int,
) -> tuple[str, dict[str, Any]]:
    """One non-streaming /api/chat call. Returns (text, raw_response)."""
    options: dict[str, Any] = {"temperature": temperature, "seed": seed}
    if num_ctx is not None:
        options["num_ctx"] = num_ctx
    payload: dict[str, Any] = {
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "stream": False,
        "options": options,
        "keep_alive": "24h",
    }
    if force_json:
        payload["format"] = "json"
    resp = http_json(f"{base_url.rstrip('/')}/api/chat", payload, timeout=timeout)
    # `resp.get("message", {})` is NOT enough: a malformed 200 response can
    # carry `"message": null`, and the `{}` default only fires on a MISSING
    # key, not an explicit null — `None.get(...)` would then raise. Coerce
    # both the message and the content through `or`.
    content = (resp.get("message") or {}).get("content") or ""
    return content, resp


def build_context(fixture: dict[str, Any]) -> str:
    """Render a fixture's compacted (model-visible) spans as a transcript."""
    lines = []
    for span in fixture.get("compacted_spans", []):
        lines.append(f"[{span.get('kind', '?')}] {span.get('content', '')}")
    return "\n\n".join(lines)


def target_fate(fixture: dict[str, Any], target_span_id: str) -> str:
    """Fate of the probe's target span under this policy: kept / summarized
    / dropped / absent (the last only if the id is unknown to the rope)."""
    for span in fixture.get("compacted_spans", []):
        if span.get("id") == target_span_id:
            return "kept"
    for span in fixture.get("removed_spans", []):
        if span.get("id") == target_span_id:
            return span.get("fate", "removed")
    return "absent"


def extract_verdict(raw: str) -> tuple[str, str]:
    try:
        obj = json.loads(raw)
        v = str(obj.get("verdict", "")).lower()
        if v in ("correct", "partial", "wrong"):
            return v, str(obj.get("reason", ""))
        return "unparsed", f"unexpected verdict {v!r}"
    except (json.JSONDecodeError, AttributeError) as exc:
        return "unparsed", repr(exc)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--fixtures", required=True, help="RopeFixture JSON (from compaction-fixtures)")
    ap.add_argument("--probes", required=True, help="probe-set JSON")
    ap.add_argument("--models", nargs="+", required=True, help="downstream model(s) to A/B")
    ap.add_argument("--judge-model", required=True, help="model that grades answers")
    ap.add_argument("--ollama", default=DEFAULT_OLLAMA)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--temperature", type=float, default=0.0)
    ap.add_argument("--num-ctx", type=int, default=None,
                    help="override num_ctx (default: omit — use the model's load-time Modelfile value)")
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--out", default=None, help="write full results JSON here (default: stdout only summary)")
    args = ap.parse_args()

    with open(args.fixtures) as fh:
        fixtures = json.load(fh)
    with open(args.probes) as fh:
        probe_doc = json.load(fh)
    probes = probe_doc["probes"]
    by_policy = {fx["policy_label"]: fx for fx in fixtures}
    if len(by_policy) != len(fixtures):
        print(f"WARNING: {len(fixtures) - len(by_policy)} duplicate policy_label(s) "
              "in fixtures — later ones silently won; check the fixture source.",
              file=sys.stderr)

    # Sanity: warn if the probes were authored for a different (session, target).
    fx_session = fixtures[0].get("session_id") if fixtures else None
    if probe_doc.get("session_id") not in (None, fx_session):
        print(f"WARNING: probes session_id={probe_doc.get('session_id')!r} "
              f"!= fixtures session_id={fx_session!r}", file=sys.stderr)
    fx_target = fixtures[0].get("target_tokens") if fixtures else None
    if probe_doc.get("target_tokens") not in (None, fx_target):
        print(f"WARNING: probes target_tokens={probe_doc.get('target_tokens')} "
              f"!= fixtures target_tokens={fx_target}", file=sys.stderr)

    results: list[dict[str, Any]] = []
    for model in args.models:
        for policy, fixture in by_policy.items():
            context = build_context(fixture)
            for probe in probes:
                user = f"CONTEXT:\n{context}\n\nQUESTION: {probe['question']}"
                started = time.time()
                try:
                    answer, _ = chat(
                        args.ollama, model, ANSWER_SYSTEM, user,
                        seed=args.seed, temperature=args.temperature,
                        num_ctx=args.num_ctx, force_json=False, timeout=args.timeout,
                    )
                    ans_err = None
                except CALL_ERRORS as exc:
                    answer, ans_err = "", repr(exc)
                elapsed = time.time() - started

                # Judge (skip if the answer call failed).
                verdict, reason = "unparsed", ans_err or ""
                if ans_err is None:
                    judge_user = (
                        f"QUESTION: {probe['question']}\n"
                        f"GOLD: {probe['gold_answer']}\n"
                        f"CANDIDATE: {answer}"
                    )
                    try:
                        jraw, _ = chat(
                            args.ollama, args.judge_model, JUDGE_SYSTEM, judge_user,
                            seed=args.seed, temperature=0.0,
                            num_ctx=args.num_ctx, force_json=True, timeout=args.timeout,
                        )
                        verdict, reason = extract_verdict(jraw)
                    except CALL_ERRORS as exc:
                        verdict, reason = "unparsed", repr(exc)

                fate = target_fate(fixture, probe["target_span_id"])
                rec = {
                    "model": model,
                    "policy": policy,
                    "probe_id": probe["id"],
                    "target_span_id": probe["target_span_id"],
                    "target_fate": fate,
                    "verdict": verdict,
                    "elapsed_s": round(elapsed, 2),
                    "answer": answer,
                    "judge_reason": reason,
                }
                results.append(rec)
                print(f"  {model:18} {policy:26} {probe['id']:12} "
                      f"fate={fate:10} -> {verdict}", file=sys.stderr)

    # Aggregate: correctness per (model, policy) and per (model, policy, fate).
    summary: dict[str, Any] = {"per_model_policy": {}, "per_model_policy_fate": {}}
    for rec in results:
        mp = f"{rec['model']}|{rec['policy']}"
        s = summary["per_model_policy"].setdefault(mp, {"correct": 0, "partial": 0, "wrong": 0, "unparsed": 0, "n": 0})
        s[rec["verdict"]] = s.get(rec["verdict"], 0) + 1
        s["n"] += 1
        mpf = f"{rec['model']}|{rec['policy']}|{rec['target_fate']}"
        sf = summary["per_model_policy_fate"].setdefault(mpf, {"correct": 0, "n": 0})
        sf["n"] += 1
        if rec["verdict"] == "correct":
            sf["correct"] += 1

    print("\n=== fidelity: correct / n  (per model | policy) ===")
    for mp, s in sorted(summary["per_model_policy"].items()):
        frac = s["correct"] / s["n"] if s["n"] else 0.0
        print(f"  {mp:48} {s['correct']}/{s['n']}  ({frac:.2f})  "
              f"[partial={s['partial']} wrong={s['wrong']} unparsed={s['unparsed']}]")
    print("\n=== correct / n  (per model | policy | target-span fate) ===")
    for mpf, s in sorted(summary["per_model_policy_fate"].items()):
        frac = s["correct"] / s["n"] if s["n"] else 0.0
        print(f"  {mpf:56} {s['correct']}/{s['n']}  ({frac:.2f})")

    if args.out:
        with open(args.out, "w") as fh:
            json.dump({
                "run_at": datetime.now(timezone.utc).isoformat(),
                "fixtures": args.fixtures,
                "probes": args.probes,
                "models": args.models,
                "judge_model": args.judge_model,
                "seed": args.seed,
                "results": results,
                "summary": summary,
            }, fh, indent=2)
        print(f"\nwrote {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
