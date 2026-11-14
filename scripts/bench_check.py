#!/usr/bin/env python3
"""Fails if a criterion benchmark has regressed beyond the threshold.

Reads the comparisons that criterion writes to target/criterion/**/change/estimates.json
after a `cargo bench -- --baseline <name>`. A benchmark counts as a regression
only if the lower bound of the confidence interval also exceeds the
threshold: that way the noise of a shared machine does not make CI fail.
"""

import argparse
import json
import pathlib
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--threshold", type=float, default=0.10)
    parser.add_argument("--dir", default="target/criterion")
    args = parser.parse_args()

    root = pathlib.Path(args.dir)
    files = sorted(root.glob("**/change/estimates.json"))
    if not files:
        print(f"no comparison found in {root}: run `cargo bench -- --baseline <name>` first")
        return 2

    regressions = []
    for path in files:
        name = str(path.parent.parent.relative_to(root))
        mean = json.loads(path.read_text())["mean"]
        change = mean["point_estimate"]
        lower = mean["confidence_interval"]["lower_bound"]
        flag = ""
        if lower > args.threshold:
            regressions.append(name)
            flag = "  <-- REGRESSION"
        print(f"{name:55} {change:+8.1%} (min {lower:+.1%}){flag}")

    if regressions:
        print(f"\n{len(regressions)} benchmark(s) regressed by more than {args.threshold:.0%}: {', '.join(regressions)}")
        return 1
    print(f"\nno regression beyond {args.threshold:.0%}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
