#!/usr/bin/env bash
# Hellfire smoke test: dedicated server + bots race to a lowered win
# score; pass = server + every bot wrote a valid JSON diag report and
# the server reported a win. PROFILE=debug for quicker iteration.
set -u
cd "$(dirname "$0")/../.."   # -> repo root

PROFILE=${PROFILE:-release}
BOTS=${BOTS:-4}
FLAG=$([ "$PROFILE" = release ] && echo --release || echo)

cargo build $FLAG -p hellfire || exit 1
BIN=target/$PROFILE/hellfire

# Hermetic: no leftovers from earlier runs, no stale mod auto-loading.
pkill -x hellfire 2>/dev/null; sleep 0.5
export HELLFIRE_NO_MODS=1

RD=$(pwd)/target/work/smoke_reports
rm -rf "$RD"; mkdir -p "$RD"

cleanup() { pkill -P $$ 2>/dev/null; pkill -x hellfire 2>/dev/null; }
trap cleanup EXIT

HELLFIRE_WIN_SCORE=500 HELLFIRE_REPORT_DIR=$RD $BIN server --quiet &
sleep 1.5
for n in $(seq "$BOTS"); do
  HELLFIRE_REPORT_DIR=$RD $BIN bot "$n" > /dev/null 2>&1 &
done

echo "smoke: server + $BOTS bots racing to 500..."
for _ in $(seq 90); do
  [ -f "$RD/server.json" ] && break
  sleep 1
done

[ -f "$RD/server.json" ] || { echo "FAIL: no server report after 90s"; exit 1; }
sleep 2  # let client reports land

python3 - "$RD" "$BOTS" << 'PY'
import json, sys, glob, os
rd, bots = sys.argv[1], int(sys.argv[2])
server = json.load(open(os.path.join(rd, "server.json")))
assert server["game_over"], "server: game not over"
assert server["win"], "server: expected a win"
assert server["score"] >= 500, f"server: score {server['score']} < 500"
assert len(server["samples"]) >= 3, "server: too few samples"
clients = [json.load(open(p)) for p in glob.glob(os.path.join(rd, "bot*.json"))]
assert len(clients) == bots, f"expected {bots} client reports, got {len(clients)}"
for c in clients:
    assert c["snapshots"] > 0, f"{c['name']}: no snapshots applied"
print(f"PASS: win at score {server['score']} in {server['duration']:.1f}s, "
      f"{len(clients)} client reports, peak {server['peak_monsters']} monsters")
PY
