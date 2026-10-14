#!/usr/bin/env python3
"""What one run cost, from run.sh's snapshots; or, with --table, a table of several runs.

report.py OUT.json NAME WITH.json APP_PID SERVER_PID|- "WS_PRE" "WS_WITH" "WS_AFTER" "XPC PIDS" "W H"
report.py --table RESULT.json...

The WS_* arguments are "min max median" of WindowServer's footprint (MB): before the app started,
while it ran, and after it quit. Its share is median(with) - median(after). WindowServer also
frees and takes back a ~110 MB block by itself when the screen is quiet, so the minimums matter
too: an app that keeps that block in use shows a high minimum while it runs.
"""
import json
import sys


def table(paths):
    rows = [json.load(open(p)) for p in paths]
    cols = [
        ("run", lambda r: r["name"]),
        ("app", lambda r: f"{r['app_mb']:.0f}"),
        ("server", lambda r: f"{r['server_mb']:.1f}" if r["server_mb"] else "-"),
        ("launchers", lambda r: f"{r['launchers_mb']:.1f} ({len(r['launchers'])}x {r['launchers'][0] if r['launchers'] else '-'})"),
        ("helpers", lambda r: f"{sum(h[1] for h in r['helpers']):.1f}"),
        ("WS share", lambda r: f"{r['ws_share_mb']:+.0f}"),
        ("WS vs before", lambda r: f"{r['ws_with'][2] - r['ws_pre'][2]:+.0f}"),
        ("WS min with/after", lambda r: f"{r['ws_with'][0]:.0f}/{r['ws_after'][0]:.0f}"),
        ("total", lambda r: f"{r['total_mb']:.0f}"),
        ("shells", lambda r: f"{r['shells_mb']:.0f}"),
        ("window", lambda r: r["window"]),
    ]
    cells = [[c[0] for c in cols]] + [[f(r) for _, f in cols] for r in rows]
    width = [max(len(row[i]) for row in cells) for i in range(len(cols))]
    for i, row in enumerate(cells):
        print("| " + " | ".join(v.ljust(width[j]) for j, v in enumerate(row)) + " |")
        if i == 0:
            print("|" + "|".join("-" * (w + 2) for w in width) + "|")
    for r in rows:
        if r["helpers"]:
            print(f"{r['name']} helpers: " + ", ".join(f"{n} {mb:.1f}" for n, mb in r["helpers"]))


def one(out, name, with_path, app, server, ws_pre, ws_with, ws_after, xpc, window):
    procs = {int(k): v for k, v in json.load(open(with_path)).items()}
    app = int(app)
    server = None if server == "-" else int(server)

    def kids(p):
        return [c for c, v in procs.items() if v["ppid"] == p]

    def tree(p):
        out, todo = [], [p]
        while todo:
            for c in kids(todo.pop()):
                out.append(c)
                todo.append(c)
        return out

    host = server or app
    launchers = kids(host)  # login (Ghostty, Terminal.app) or ctx-launch (ctx)
    shells = [s for l in launchers for s in kids(l)]
    workload = [w for s in shells for w in tree(s)]
    mb = lambda p: procs[p]["mb"] if p in procs else 0.0
    helpers = [(procs[p]["cmd"], mb(p)) for p in map(int, xpc.split()) if p in procs]
    ws = lambda s: [float(x) for x in s.split()]
    pre, with_, after = ws(ws_pre), ws(ws_with), ws(ws_after)
    r = {
        "name": name,
        "app_mb": mb(app),
        "server_mb": mb(server) if server else 0.0,
        "launchers": [procs[l]["cmd"].split()[0] for l in launchers],
        "launchers_mb": sum(mb(l) for l in launchers),
        "shells_mb": sum(mb(s) for s in shells),
        "workload_mb": sum(mb(w) for w in workload),
        "helpers": helpers,
        "ws_pre": pre,
        "ws_with": with_,
        "ws_after": after,
        "ws_share_mb": with_[2] - after[2],
        "window": window,
    }
    r["total_mb"] = (
        r["app_mb"] + r["server_mb"] + r["launchers_mb"] + sum(h[1] for h in helpers) + max(r["ws_share_mb"], 0)
    )
    json.dump(r, open(out, "w"), indent=1)
    table([out])


if __name__ == "__main__":
    if sys.argv[1] == "--table":
        table(sys.argv[2:])
    else:
        one(*sys.argv[1:])
