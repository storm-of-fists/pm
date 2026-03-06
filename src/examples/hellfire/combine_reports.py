#!/usr/bin/env python3
"""Combine hellfire diagnostic reports into a single summary.

Usage:
    python3 combine_reports.py [report_dir]

Defaults to work/reports/ relative to the project root.
Outputs a combined JSON to stdout and prints a human-readable summary to stderr.
"""
import json
import os
import sys
import statistics

def load_reports(report_dir):
    reports = {}
    for fname in sorted(os.listdir(report_dir)):
        if not fname.endswith(".json"):
            continue
        path = os.path.join(report_dir, fname)
        with open(path) as f:
            reports[fname] = json.load(f)
    return reports

def summarize(reports):
    server = None
    clients = []
    for fname, r in reports.items():
        if r.get("role") == "server":
            server = r
        else:
            clients.append(r)

    out = {"server": None, "clients": [], "timeline_frames": 0}

    # Server summary
    if server:
        tl = server.get("timeline", [])
        frame_times = [s["frame_ms"] for s in tl if s["frame_ms"] > 0]
        out["server"] = {
            "duration": server.get("duration", 0),
            "frames": server.get("frames", 0),
            "outcome": server.get("outcome", {}),
            "entities": server.get("entities", {}),
            "timing": server.get("timing", {}),
            "events": len(server.get("events", [])),
            "timeline_samples": len(tl),
            "peers": server.get("peers", []),
        }
        if frame_times:
            out["server"]["timeline_frame_ms"] = {
                "min": round(min(frame_times), 3),
                "max": round(max(frame_times), 3),
                "median": round(statistics.median(frame_times), 3),
                "p95": round(sorted(frame_times)[int(len(frame_times) * 0.95)], 3),
            }

    # Client summaries
    for c in sorted(clients, key=lambda x: x.get("name", "")):
        tl = c.get("timeline", [])
        frame_times = [s["frame_ms"] for s in tl if s["frame_ms"] > 0]
        snap_ages = [s.get("snap_age_ms", 0) for s in tl]
        net = c.get("network", {})
        entry = {
            "name": c.get("name", "?"),
            "peer_id": c.get("peer_id", -1),
            "duration": c.get("duration", 0),
            "frames": c.get("frames", 0),
            "outcome": c.get("outcome", {}),
            "entities": c.get("entities", {}),
            "timing": c.get("timing", {}),
            "network": net,
            "events": len(c.get("events", [])),
            "timeline_samples": len(tl),
        }
        if frame_times:
            entry["timeline_frame_ms"] = {
                "min": round(min(frame_times), 3),
                "max": round(max(frame_times), 3),
                "median": round(statistics.median(frame_times), 3),
                "p95": round(sorted(frame_times)[int(len(frame_times) * 0.95)], 3),
            }
        if any(a > 0 for a in snap_ages):
            nonzero = [a for a in snap_ages if a > 0]
            entry["timeline_snap_age_ms"] = {
                "min": round(min(nonzero), 2),
                "max": round(max(nonzero), 2),
                "median": round(statistics.median(nonzero), 2),
            }
        out["clients"].append(entry)
        out["timeline_frames"] += len(tl)

    if server:
        out["timeline_frames"] += len(server.get("timeline", []))

    return out

def print_summary(s):
    p = lambda *a: print(*a, file=sys.stderr)
    p("=" * 72)
    p("HELLFIRE DIAGNOSTIC SUMMARY")
    p("=" * 72)

    if s["server"]:
        sv = s["server"]
        p(f"\nSERVER  duration={sv['duration']:.1f}s  frames={sv['frames']}")
        oc = sv["outcome"]
        p(f"  score={oc.get('score',0)}  kills={oc.get('kills',0)}  "
          f"level={oc.get('level',0)}  game_over={oc.get('game_over')}  win={oc.get('win')}")
        ent = sv["entities"]
        p(f"  peak: monsters={ent.get('peak_monsters',0)}  "
          f"bullets={ent.get('peak_bullets',0)}  players={ent.get('peak_players',0)}")
        p(f"  spawns={ent.get('total_spawns',0)}  removes={ent.get('total_removes',0)}")
        if "timeline_frame_ms" in sv:
            t = sv["timeline_frame_ms"]
            p(f"  frame_ms: min={t['min']}  median={t['median']}  "
              f"p95={t['p95']}  max={t['max']}")
        p(f"  events={sv['events']}  timeline_samples={sv['timeline_samples']}")
        if sv.get("peers"):
            p(f"  peers:")
            for peer in sv["peers"]:
                p(f"    id={peer['id']} name={peer['name']} "
                  f"connected_at={peer['connected_at']:.2f} alive={peer['alive_at_end']}")

    p(f"\nCLIENTS ({len(s['clients'])})")
    p(f"  {'name':<8} {'peer':>4} {'dur':>6} {'frames':>7} "
      f"{'min_ms':>7} {'med_ms':>7} {'p95_ms':>7} {'max_ms':>7} "
      f"{'snap_med':>8} {'snap_max':>8} {'events':>6}")
    p(f"  {'-'*8} {'-'*4} {'-'*6} {'-'*7} "
      f"{'-'*7} {'-'*7} {'-'*7} {'-'*7} "
      f"{'-'*8} {'-'*8} {'-'*6}")
    for c in s["clients"]:
        ft = c.get("timeline_frame_ms", {})
        sa = c.get("timeline_snap_age_ms", {})
        p(f"  {c['name']:<8} {c['peer_id']:>4} {c['duration']:>6.1f} {c['frames']:>7} "
          f"{ft.get('min',''):>7} {ft.get('median',''):>7} "
          f"{ft.get('p95',''):>7} {ft.get('max',''):>7} "
          f"{sa.get('median',''):>8} {sa.get('max',''):>8} "
          f"{c['events']:>6}")

    p(f"\ntotal timeline frames across all reports: {s['timeline_frames']}")
    p("=" * 72)

def main():
    script_dir = os.path.dirname(os.path.abspath(__file__))
    project_root = os.path.abspath(os.path.join(script_dir, "..", "..", ".."))
    default_dir = os.path.join(project_root, "work", "reports")

    report_dir = sys.argv[1] if len(sys.argv) > 1 else default_dir

    if not os.path.isdir(report_dir):
        print(f"ERROR: report directory not found: {report_dir}", file=sys.stderr)
        sys.exit(1)

    reports = load_reports(report_dir)
    if not reports:
        print(f"ERROR: no .json files in {report_dir}", file=sys.stderr)
        sys.exit(1)

    summary = summarize(reports)
    print_summary(summary)

    # Full combined JSON to stdout
    combined = {"summary": summary, "raw": reports}
    json.dump(combined, sys.stdout, indent=2)
    print()

if __name__ == "__main__":
    main()
