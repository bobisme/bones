#!/usr/bin/env python3
"""Fail CI when large-repo SLO benchmark lines report regressions."""

from __future__ import annotations

import re
import sys
from pathlib import Path


LINE_RE = re.compile(
    r"^SLO\s+tier=(?P<tier>\S+)\s+op=(?P<op>\S+)\s+.*\s+target=(?P<target>\S+)\s+(?P<status>PASS|FAIL)\s*$"
)

REQUIRED_OPS = {
    "list_open",
    "incremental_apply_10",
    "full_rebuild",
}


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check_large_repo_slo.py <bench-log>", file=sys.stderr)
        return 2

    log_path = Path(sys.argv[1])
    if not log_path.exists():
        print(f"benchmark log not found: {log_path}", file=sys.stderr)
        return 2

    statuses: dict[str, str] = {}
    targets: dict[str, str] = {}

    for line in log_path.read_text(encoding="utf-8", errors="replace").splitlines():
        m = LINE_RE.match(line.strip())
        if not m:
            continue
        op = m.group("op")
        statuses[op] = m.group("status")
        targets[op] = m.group("target")

    missing = sorted(REQUIRED_OPS - statuses.keys())
    if missing:
        print(
            "missing required SLO benchmark output for: " + ", ".join(missing),
            file=sys.stderr,
        )
        return 1

    failed = [
        op for op, status in statuses.items() if op in REQUIRED_OPS and status == "FAIL"
    ]
    if failed:
        print("large-repo benchmark SLO regression detected:", file=sys.stderr)
        for op in failed:
            print(f"  - {op} exceeded target {targets.get(op, '?')}", file=sys.stderr)
        return 1

    print("large-repo benchmark SLOs passed:")
    for op in sorted(REQUIRED_OPS):
        print(f"  - {op}: PASS (target {targets[op]})")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
