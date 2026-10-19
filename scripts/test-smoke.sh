#!/usr/bin/env bash
# scripts/test-smoke.sh
#
# Functional smoke test for a deployed mibee-eye instance.
# Runs over SSH against the target device. Exits non-zero on any failure.
#
# Usage:
#   ./scripts/test-smoke.sh <host> [camera_id]
#
# If camera_id is omitted, uses the first camera from /api/cameras.

set -euo pipefail

HOST="${1:?Usage: $0 <host> [camera_id]}"
CAM_ID="${2:-}"

FAIL=0
pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; FAIL=1; }

echo "=== Smoke test: $HOST ==="

# ── 1. Service is active ──────────────────────────────────────────────────────
if ssh "$HOST" "systemctl --user is-active mibee-eye" | grep -q "^active$"; then
    pass "systemd service is active"
else
    fail "systemd service is NOT active"
    ssh "$HOST" "systemctl --user status mibee-eye --no-pager -l | head -30" >&2 || true
    exit 1
fi

# ── 2. /health returns 200 ───────────────────────────────────────────────────
if ssh "$HOST" "curl -skf -o /dev/null -w '%{http_code}' https://localhost:8443/health" | grep -q "^200$"; then
    pass "/health returns 200"
else
    fail "/health did not return 200"
fi

# ── 3. /metrics has no panic/error markers ────────────────────────────────────
METRICS=$(ssh "$HOST" "curl -skf https://localhost:8443/metrics" 2>/dev/null || echo "")
if [ -n "$METRICS" ]; then
    pass "/metrics is reachable"
    if echo "$METRICS" | grep -qiE "panic|^# .*error"; then
        fail "/metrics contains panic/error markers"
    else
        pass "/metrics has no panic/error markers"
    fi
else
    fail "/metrics returned empty"
fi

# ── 4. Discover a camera_id if not given ──────────────────────────────────────
if [ -z "$CAM_ID" ]; then
    CAM_ID=$(ssh "$HOST" "curl -skf https://localhost:8443/api/cameras" 2>/dev/null \
        | grep -oE '"camera_id":"[^"]+"' | head -1 | cut -d'"' -f4 || echo "")
    if [ -z "$CAM_ID" ]; then
        echo "  (no cameras registered — skipping camera-specific tests)"
        echo ""
        if [ "$FAIL" -ne 0 ]; then exit 1; fi
        echo "✓ Smoke test passed (service-level only)."
        exit 0
    fi
fi
echo "  Using camera_id: $CAM_ID"

# ── 5. Start the stream if not running ────────────────────────────────────────
STREAM_STATE=$(ssh "$HOST" "curl -skf https://localhost:8443/api/cameras/$CAM_ID" 2>/dev/null \
    | grep -oE '"status":"[^"]+"' | head -1 | cut -d'"' -f4 || echo "")
if [ "$STREAM_STATE" != "running" ]; then
    echo "  → Starting stream..."
    ssh "$HOST" "curl -skf -X POST https://localhost:8443/api/cameras/$CAM_ID/start" >/dev/null 2>&1 || true
    sleep 3
fi

# ── 6. RTSP DESCRIBE returns SDP (native H.264 feeds the RTSP server) ─────────
RTSP_URL="rtsp://127.0.0.1:8554/live/$CAM_ID"
SDP=$(ssh "$HOST" "curl -s --url \"$RTSP_URL\" --request DESCRIBE --header 'CSeq: 1' 2>/dev/null || echo ''")
if echo "$SDP" | grep -qiE "H264/90000|m=video"; then
    pass "RTSP DESCRIBE returns video SDP (H.264)"
else
    fail "RTSP DESCRIBE did not return a valid H.264 video SDP"
fi

# ── 7. Snapshot returns a valid JPEG ──────────────────────────────────────────
SNAP=$(ssh "$HOST" "curl -skf -o /tmp/mibee-snap.jpg -w '%{http_code}' https://localhost:8443/api/cameras/$CAM_ID/snapshot" 2>/dev/null || echo "000")
if [ "$SNAP" = "200" ]; then
    MAGIC=$(ssh "$HOST" "head -c 2 /tmp/mibee-snap.jpg | xxd -p" 2>/dev/null || echo "")
    if [ "$MAGIC" = "ffd8" ]; then
        pass "snapshot endpoint returns valid JPEG (FF D8 magic)"
    else
        fail "snapshot returned 200 but body is not JPEG (magic=$MAGIC)"
    fi
else
    fail "snapshot endpoint returned HTTP $SNAP"
fi

# ── 8. Live preview emits a multipart boundary ────────────────────────────────
PREVIEW=$(ssh "$HOST" "timeout 3 curl -skf https://localhost:8443/api/cameras/$CAM_ID/live 2>/dev/null | head -c 200 || echo ''")
if echo "$PREVIEW" | grep -qiE "multipart/x-mixed-replace|--mibeejpeg|Content-Type: image/jpeg"; then
    pass "live preview emits multipart MJPEG frames"
else
    fail "live preview did not emit expected multipart boundary"
fi

# ── 9. CRITICAL: zero ffmpeg references in journal ────────────────────────────
# Both unit names are matched: test rigs run mibee-eye-test, fleet hosts run
# mibee-eye — querying only one made this check pass vacuously on the other.
FFMPEG_HITS=$(ssh "$HOST" "journalctl --user -u mibee-eye-test -u mibee-eye --since '5 min ago' --no-pager 2>/dev/null | grep -ci 'ffmpeg' || echo 0")
if [ "$FFMPEG_HITS" = "0" ]; then
    pass "ZERO ffmpeg references in journal (full removal confirmed)"
else
    fail "journal contains $FFMPEG_HITS ffmpeg references (removal incomplete?)"
fi

# ── 10. Build provenance: startup banner pins sha, clean build (mibee-rec #1) ──
# SMOKE_EXPECT_SHA=<sha> additionally pins the deployed binary to an exact
# commit (machine-checked "deployed == expected", no mtime archaeology).
# Boot-bounded (-b): the banner logs once at startup, so any rolling window
# would miss it on a long-running service; the current boot always has it.
BANNER=$(ssh "$HOST" "journalctl --user -u mibee-eye-test -u mibee-eye -b --no-pager 2>/dev/null | grep -o 'mibee-eye starting.*' | tail -1" || echo '')
RUN_SHA=$(echo "$BANNER" | grep -oP 'sha: "\K[0-9a-f]{7,40}' || echo '')
RUN_DIRTY=$(echo "$BANNER" | grep -oP 'dirty: \K[a-z]+' || echo '')
if [ -n "$RUN_SHA" ] && [ "$RUN_DIRTY" = "false" ]; then
    pass "build provenance: sha=$RUN_SHA dirty=false"
    if [ -n "$SMOKE_EXPECT_SHA" ] && [ "$RUN_SHA" != "$SMOKE_EXPECT_SHA" ]; then
        fail "deployed sha $RUN_SHA != expected SMOKE_EXPECT_SHA=$SMOKE_EXPECT_SHA"
    fi
else
    fail "build provenance banner missing/invalid (sha='$RUN_SHA' dirty='$RUN_DIRTY')"
fi

echo ""
if [ "$FAIL" -ne 0 ]; then
    echo "✗ Smoke test FAILED on $HOST." >&2
    exit 1
fi
echo "✓ Smoke test PASSED on $HOST."
