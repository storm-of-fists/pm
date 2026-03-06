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

def fmt_bytes(n):
    if n >= 1_000_000: return f"{n/1_000_000:.1f}MB"
    if n >= 1_000: return f"{n/1_000:.1f}KB"
    return f"{n}B"

def load_reports(report_dir):
    reports = {}
    for fname in sorted(os.listdir(report_dir)):
        if not fname.endswith(".json"):
            continue
        path = os.path.join(report_dir, fname)
        with open(path) as f:
            reports[fname] = json.load(f)
    return reports

def percentile(data, pct):
    if not data: return 0
    s = sorted(data)
    return s[min(int(len(s) * pct / 100), len(s) - 1)]

def tl_stats(timeline, key):
    vals = [s.get(key, 0) for s in timeline]
    nonzero = [v for v in vals if v > 0]
    if not nonzero:
        return {}
    return {
        "min": round(min(nonzero), 3),
        "max": round(max(nonzero), 3),
        "median": round(statistics.median(nonzero), 3),
        "p95": round(percentile(nonzero, 95), 3),
        "sum": round(sum(vals), 3),
    }

def summarize(reports):
    server = None
    clients = []
    for fname, r in reports.items():
        if r.get("role") == "server":
            server = r
        else:
            clients.append(r)

    out = {"server": None, "clients": [], "timeline_frames": 0}

    if server:
        tl = server.get("timeline", [])
        net = server.get("network", {})
        out["server"] = {
            "duration": server.get("duration", 0),
            "frames": server.get("frames", 0),
            "outcome": server.get("outcome", {}),
            "entities": server.get("entities", {}),
            "timing": server.get("timing", {}),
            "network": net,
            "events": len(server.get("events", [])),
            "timeline_samples": len(tl),
            "peers": server.get("peers", []),
            "tl_frame_ms": tl_stats(tl, "frame_ms"),
            "tl_rtt_ms": tl_stats(tl, "rtt_ms"),
            "tl_bytes_sent": tl_stats(tl, "bytes_sent"),
            "tl_bytes_recv": tl_stats(tl, "bytes_recv"),
        }
        out["timeline_frames"] += len(tl)

    for c in sorted(clients, key=lambda x: x.get("name", "")):
        tl = c.get("timeline", [])
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
            "tl_frame_ms": tl_stats(tl, "frame_ms"),
            "tl_rtt_ms": tl_stats(tl, "rtt_ms"),
            "tl_snap_age_ms": tl_stats(tl, "snap_age_ms"),
            "tl_bytes_sent": tl_stats(tl, "bytes_sent"),
            "tl_bytes_recv": tl_stats(tl, "bytes_recv"),
        }
        out["clients"].append(entry)
        out["timeline_frames"] += len(tl)

    return out

def print_summary(s):
    p = lambda *a: print(*a, file=sys.stderr)
    p("=" * 100)
    p("HELLFIRE DIAGNOSTIC SUMMARY")
    p("=" * 100)

    if s["server"]:
        sv = s["server"]
        net = sv.get("network", {})
        p(f"\nSERVER  duration={sv['duration']:.1f}s  frames={sv['frames']}")
        oc = sv["outcome"]
        p(f"  score={oc.get('score',0)}  kills={oc.get('kills',0)}  "
          f"level={oc.get('level',0)}  game_over={oc.get('game_over')}  win={oc.get('win')}")
        ent = sv["entities"]
        p(f"  peak: monsters={ent.get('peak_monsters',0)}  "
          f"bullets={ent.get('peak_bullets',0)}  players={ent.get('peak_players',0)}")
        p(f"  spawns={ent.get('total_spawns',0)}  removes={ent.get('total_removes',0)}")
        ft = sv.get("tl_frame_ms", {})
        if ft:
            p(f"  frame_ms: min={ft['min']}  median={ft['median']}  "
              f"p95={ft['p95']}  max={ft['max']}")
        rt = sv.get("tl_rtt_ms", {})
        if rt:
            p(f"  rtt_ms:   min={rt['min']}  median={rt['median']}  "
              f"p95={rt['p95']}  max={rt['max']}")
        p(f"  bandwidth: sent={fmt_bytes(net.get('bytes_sent',0))}  "
          f"recv={fmt_bytes(net.get('bytes_recv',0))}  "
          f"packets_out={net.get('packets_sent',0)}  packets_in={net.get('packets_recv',0)}")
        p(f"  events={sv['events']}  timeline_samples={sv['timeline_samples']}")

        if sv.get("peers"):
            p(f"\n  PEERS (server view)")
            p(f"  {'name':<8} {'id':>3} {'conn_at':>8} {'alive':>6} "
              f"{'rtt_ms':>7} {'rtt_n':>6} {'pkts':>7}")
            p(f"  {'-'*8} {'-'*3} {'-'*8} {'-'*6} {'-'*7} {'-'*6} {'-'*7}")
            for peer in sv["peers"]:
                p(f"  {peer['name']:<8} {peer['id']:>3} "
                  f"{peer['connected_at']:>8.2f} {'yes' if peer['alive_at_end'] else 'no':>6} "
                  f"{peer.get('rtt_ms', 0):>7.2f} {peer.get('rtt_samples', 0):>6} "
                  f"{peer.get('packets_sent', 0):>7}")

    p(f"\nCLIENTS ({len(s['clients'])})")
    p(f"  {'name':<8} {'peer':>4} {'dur':>6} {'frames':>7} "
      f"{'frm_med':>7} {'frm_p95':>7} "
      f"{'rtt_med':>7} {'rtt_p95':>7} "
      f"{'sent':>8} {'recv':>8} "
      f"{'clk_min':>8} {'clk_max':>8}")
    p(f"  {'-'*8} {'-'*4} {'-'*6} {'-'*7} "
      f"{'-'*7} {'-'*7} "
      f"{'-'*7} {'-'*7} "
      f"{'-'*8} {'-'*8} "
      f"{'-'*8} {'-'*8}")
    for c in s["clients"]:
        ft = c.get("tl_frame_ms", {})
        rt = c.get("tl_rtt_ms", {})
        net = c.get("network", {})
        p(f"  {c['name']:<8} {c['peer_id']:>4} {c['duration']:>6.1f} {c['frames']:>7} "
          f"{ft.get('median',''):>7} {ft.get('p95',''):>7} "
          f"{rt.get('median',''):>7} {rt.get('p95',''):>7} "
          f"{fmt_bytes(net.get('bytes_sent',0)):>8} {fmt_bytes(net.get('bytes_recv',0)):>8} "
          f"{net.get('clock_offset_min',0):>8.4f} {net.get('clock_offset_max',0):>8.4f}")

    p(f"\ntotal timeline frames across all reports: {s['timeline_frames']}")
    p("=" * 100)

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
