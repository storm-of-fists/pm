#!/bin/bash
# Smoke test: launch hellfire server + 8 AI bot clients
# Each bot opens a visible SDL window, moves randomly, shoots at monsters.
# Exits automatically when all players die (game over).
# Validates diagnostic reports from server + all clients.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
BUILD_DIR="$PROJECT_ROOT/build"

# Build
echo "[smoke] building..."
cmake --build "$BUILD_DIR" --target hellfire_server hellfire_client 2>&1

# Find client binary (CMake may place it in build root or a subdirectory)
CLIENT="$(find "$BUILD_DIR" -name hellfire_client -type f -executable | head -1)"
if [ -z "$CLIENT" ]; then
    echo "[smoke] ERROR: hellfire_client not found in $BUILD_DIR"
    exit 1
fi
echo "[smoke] using $CLIENT"

# Report directory for diagnostics (inside work/ so it's gitignored)
export HELLFIRE_REPORT_DIR="$PROJECT_ROOT/work/reports"
rm -rf "$HELLFIRE_REPORT_DIR"
mkdir -p "$HELLFIRE_REPORT_DIR"
echo "[smoke] reports → $HELLFIRE_REPORT_DIR"

PIDS=()
cleanup() {
    echo "[smoke] cleaning up..."
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null
}
trap cleanup EXIT

# Launch host bot (spawns server internally, waits for 8 players then starts)
echo "[smoke] launching host bot..."
"$CLIENT" --bot --host --name Bot1 &
PIDS+=($!)
sleep 1

# Launch 7 joiner bots
for i in 2 3 4 5 6 7 8; do
    echo "[smoke] launching Bot$i..."
    "$CLIENT" --bot --name "Bot$i" &
    PIDS+=($!)
    sleep 0.3
done

echo "[smoke] all 8 bots running — watch them play!"
echo "[smoke] game will exit automatically when all players die."

# Wait for host bot (when it exits, server dies, joiners follow)
wait "${PIDS[0]}" 2>/dev/null || true
sleep 2

echo "[smoke] done."
echo ""

# ── Report validation ────────────────────────────────────────────────────────
echo "[smoke] validating diagnostic reports..."
FAILS=0

check() {
    local file="$1" pattern="$2" desc="$3"
    if grep -q "$pattern" "$file" 2>/dev/null; then
        echo "  PASS: $desc"
    else
        echo "  FAIL: $desc"
        FAILS=$((FAILS + 1))
    fi
}

check_file() {
    local file="$1" desc="$2"
    if [ -f "$file" ]; then
        echo "  PASS: $desc"
    else
        echo "  FAIL: $desc"
        FAILS=$((FAILS + 1))
    fi
}

# Server report
check_file "$HELLFIRE_REPORT_DIR/server.json" "server report exists"
check "$HELLFIRE_REPORT_DIR/server.json" '"role": "server"'       "server: correct role"
check "$HELLFIRE_REPORT_DIR/server.json" '"game_over": true'      "server: game ended"
check "$HELLFIRE_REPORT_DIR/server.json" '"peak_monsters":'       "server: tracked monsters"
check "$HELLFIRE_REPORT_DIR/server.json" '"peak_bullets":'        "server: tracked bullets"
check "$HELLFIRE_REPORT_DIR/server.json" '"peak_players": 8'      "server: all 8 players seen"
check "$HELLFIRE_REPORT_DIR/server.json" '"timeline": \['         "server: has timeline"
check "$HELLFIRE_REPORT_DIR/server.json" '"events": \['           "server: has events"
check "$HELLFIRE_REPORT_DIR/server.json" '"game started'          "server: game start event"

# Client reports
for i in 1 2 3 4 5 6 7 8; do
    F="$HELLFIRE_REPORT_DIR/Bot$i.json"
    check_file "$F" "Bot$i report exists"
    check "$F" '"role": "client"'       "Bot$i: correct role"
    check "$F" '"game_over": true'      "Bot$i: saw game over"
    check "$F" '"frames":'              "Bot$i: has frame data"
    check "$F" '"snapshot_age_avg_ms":' "Bot$i: has network stats"
    check "$F" '"timeline": \['         "Bot$i: has timeline"
done

echo ""
if [ "$FAILS" -gt 0 ]; then
    echo "[smoke] $FAILS check(s) FAILED"
    echo "[smoke] reports in: $HELLFIRE_REPORT_DIR"
    exit 1
fi

echo "[smoke] all checks passed!"
echo "[smoke] reports in: $HELLFIRE_REPORT_DIR"
