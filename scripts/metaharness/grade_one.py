#!/usr/bin/env python3
"""grade_one — grade ONE agentic answer by REUSING agentic-bench's grader.

The profile-search loop (profile-search.sh) runs each agentic case through
agent-dispatch.sh and captures the answer. Grading must be byte-identical to the
benchmark's own, so we import arch_score.grade_agentic rather than reimplement the
regex/leak/fabrication logic — collect-separated-from-grade, the same discipline
arch_score uses (cheap, deterministic, re-runnable).

usage: grade_one.py <case.json> <answer-file>
  prints the grade dict as JSON: {score, correct, leak, fabricated, points, max_points}
env: ARCH_BENCH (agentic-bench checkout; default ~/src/public_github/agentic-bench)
"""
import json
import os
import pathlib
import sys

AB = os.environ.get("ARCH_BENCH",
                    str(pathlib.Path.home() / "src/public_github/agentic-bench"))
sys.path.insert(0, AB)
import arch_score  # noqa: E402  (path set above)


def main() -> int:
    if len(sys.argv) != 3:
        print('{"score": null, "ungraded": "bad-args"}')
        return 2
    case = json.loads(pathlib.Path(sys.argv[1]).read_text())
    answer = pathlib.Path(sys.argv[2]).read_text(errors="ignore")
    # Build the minimal row arch_score.grade_agentic consumes.
    row = {"task_type": "agentic", "answer": answer,
           "answer_regex": case.get("answer_regex", ""),
           "negative": case.get("negative", False)}
    print(json.dumps(arch_score.grade_agentic(row)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
