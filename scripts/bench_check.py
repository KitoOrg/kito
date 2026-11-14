#!/usr/bin/env python3
"""Fallisce se un benchmark criterion è peggiorato oltre la soglia.

Legge i confronti che criterion scrive in target/criterion/**/change/estimates.json
dopo un `cargo bench -- --baseline <nome>`. Un benchmark conta come regressione
solo se anche l'estremo inferiore dell'intervallo di confidenza supera la
soglia: così il rumore di una macchina condivisa non fa fallire la CI.
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
        print(f"nessun confronto trovato in {root}: lancia prima `cargo bench -- --baseline <nome>`")
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
            flag = "  <-- REGRESSIONE"
        print(f"{name:55} {change:+8.1%} (minimo {lower:+.1%}){flag}")

    if regressions:
        print(f"\n{len(regressions)} benchmark peggiorati di oltre il {args.threshold:.0%}: {', '.join(regressions)}")
        return 1
    print(f"\nnessuna regressione oltre il {args.threshold:.0%}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
