#!/usr/bin/env bash
# scripts/deploy.sh
#
# Deploy the cross-compiled mibee-eye binary to a target device over SSH.
# Does NOT start/restart the service — run scripts/service.sh after this.
#
# Usage:
#   ./scripts/deploy.sh device-1
#   ./scripts/deploy.sh device-2
#
# Prerequisites:
#   - Run scripts/docker-build.sh first (produces target/linux-x86_64/)
#   - SSH config entries for the host aliases (in ~/.ssh/config)
#   - SSH key auth set up for <user>@<host>

set -euo pipefail

cd "$(dirname "$0")/.."

HOST="${1:-}"
OUT_DIR="target/linux-x86_64"
# Use ~ (tilde) — both ssh and scp expand it to the remote user's home.
REMOTE_DIR="~/mibee-eye"

if [ -z "$HOST" ]; then
    echo "Usage: $0 <ssh-host-alias>"
    echo ""
    echo "Host aliases are read from ~/.ssh/config, e.g.:"
    echo "  device-1"
    echo "  device-2"
    exit 1
fi

# ── 1. Verify build artifacts exist ───────────────────────────────────────────
if [ ! -f "$OUT_DIR/mibee-eye" ]; then
    echo "ERROR: $OUT_DIR/mibee-eye not found." >&2
    echo "       Run ./scripts/docker-build.sh first." >&2
    exit 1
fi
if [ ! -d "$OUT_DIR/migrations" ]; then
    echo "ERROR: $OUT_DIR/migrations/ not found." >&2
    exit 1
fi

echo "→ Deploying to $HOST..."

# ── 2. Prepare remote directory ───────────────────────────────────────────────
ssh "$HOST" "mkdir -p $REMOTE_DIR/migrations"

# ── 3. Copy binary + migrations + config ──────────────────────────────────────
echo "  Copying binary..."
scp -q "$OUT_DIR/mibee-eye" "$HOST:$REMOTE_DIR/mibee-eye.new"

echo "  Copying migrations..."
scp -q -r "$OUT_DIR/migrations/"* "$HOST:$REMOTE_DIR/migrations/"

echo "  Config (seed-only-if-absent — never clobbers the operator's config.local.toml)..."
ssh "$HOST" "test -f $REMOTE_DIR/config.local.toml" 2>/dev/null \
  || scp -q "$OUT_DIR/config.toml" "$HOST:$REMOTE_DIR/config.local.toml"
echo "  Tip: on the device, \`$REMOTE_DIR/mibee-eye --migrate-config\` merges new default keys."

# ── 4. Atomic swap + backup rotation ─────────────────────────────────────────
# Move the new binary into place atomically. If the service is running, the
# old inode stays open until restart; the new file is picked up on next start.
# The replaced binary becomes a dated .bak, pruned to the newest 3 —
# unbounded .bak piles cost ~95MB each (mibee-rec #1).
ssh "$HOST" "cd $REMOTE_DIR && if [ -f mibee-eye ]; then cp -p mibee-eye mibee-eye.bak-\$(date +%Y%m%d-%H%M); ls -1t mibee-eye.bak-* 2>/dev/null | tail -n +4 | xargs -r rm -f; fi && chmod +x mibee-eye.new && mv -f mibee-eye.new mibee-eye"

# ── 5. Verify camera device access ───────────────────────────────────────────
echo "  Verifying camera device access..."
ssh "$HOST" "ls -l /dev/video0 2>/dev/null || echo '  NOTE: /dev/video0 not present (camera may be disconnected)'"

# ── 6. Verify the user is in the video/audio groups ──────────────────────────
ssh "$HOST" "id | grep -oE '(video|audio)' | sort -u || echo '  WARNING: user not in video/audio groups — run: sudo usermod -aG video,audio \$USER && re-login'"

echo ""
echo "✓ Deployed to $HOST."
echo "  Binary:    \$HOME/mibee-eye/mibee-eye"
echo "  Config:    \$HOME/mibee-eye/config.local.toml"
echo ""
echo "Next: ./scripts/service.sh $HOST start"
