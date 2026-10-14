#!/usr/bin/env python3
"""snap.py OUT.json [SAMPLES]: every process's physical footprint (what Activity Monitor shows),
the median of SAMPLES one-second `top` samples, with its parent and command."""
import collections
import json
import statistics
import subprocess
import sys

samples = sys.argv[2] if len(sys.argv) > 2 else "5"
UNITS = {"B": 1 / 1048576, "K": 1 / 1024, "M": 1, "G": 1024}


def mb(s):
    s = s.rstrip("+-")
    return float(s[:-1]) * UNITS[s[-1]]


seen = collections.defaultdict(list)
info = {}
out = subprocess.run(
    ["top", "-l", samples, "-s", "1", "-stats", "pid,ppid,mem,command"], capture_output=True, text=True
).stdout
for line in out.splitlines():
    p = line.split(None, 3)
    if len(p) < 4 or not p[0].isdigit():
        continue
    try:
        seen[int(p[0])].append(mb(p[2]))
        info[int(p[0])] = (int(p[1]), p[3].strip())
    except (ValueError, KeyError, IndexError):
        pass
json.dump(
    {p: {"ppid": info[p][0], "cmd": info[p][1], "mb": statistics.median(v)} for p, v in seen.items()},
    open(sys.argv[1], "w"),
)
