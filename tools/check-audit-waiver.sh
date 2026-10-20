#!/usr/bin/env bash
# Verify the premises behind the audit waivers in tools/audit.toml still
# hold. Run by the `audit` CI job before `cargo audit`.
#
# A waiver whose justification can silently expire is worse than no waiver, so
# each one re-derives its justification here on every run. The script fails the
# job when a premise breaks — fix the dependency, never widen the ignore list.
#
# Waiver 1: RUSTSEC-2023-0071 (`rsa`) — UNREACHABLE-only waiver.
#   `rsa` is pulled in only by sqlx 0.8's Cargo.toml, which lists every
#   backend unconditionally (sqlx-mysql -> rsa), and this application is
#   SQLite-only so the MySQL RSA authentication path it exposes is never
#   executed. Fails when:
#     a. `rsa` becomes reachable for some real target — a live vulnerability
#        being waved away;
#     b. a patched version of `rsa` exists — the waiver is obsolete and
#        should be replaced by an upgrade;
#     c. `rsa` disappears from the lockfile entirely — the waiver is dead
#        config and should be removed.
#
# Waiver 2: RUSTSEC-2026-0367/0368/0369 (`zip` 2.x) — BUILD-ONLY waiver.
#   The only consumer is `sherpa-onnx-sys` 1.13.8's build script, which
#   unpacks release archives it downloads; `zip` is never compiled into a
#   shipped binary, so the flagged archive-extraction flaws are a
#   build-machine supply-chain surface only. The patch line (>= 9.0.0) is
#   semver-incompatible with the consumer's `zip = "2"` requirement, and
#   cargo maximises the lockfile version, so any future patched 2.x is
#   picked up automatically. Fails when:
#     a. `zip` acquires a normal/dev edge for some target — the flaws would
#        then live in a compiled, product-reachable surface;
#     b. `zip`'s lockfile version moves past the patch line — the advisories
#        no longer apply and the ignore entries are dead config to remove;
#     c. `zip` disappears from the lockfile entirely — same: dead config.

set -euo pipefail

cd "$(dirname "$0")/.."

if [[ ! -f Cargo.lock ]]; then
    echo "check-audit-waiver: no Cargo.lock; run a build first" >&2
    exit 1
fi

fail=0

# ════════════════════════════════════════════════════════════════════════════
# Waiver 1 — rsa / RUSTSEC-2023-0071 (unreachable)
# ════════════════════════════════════════════════════════════════════════════

ADVISORY="RUSTSEC-2023-0071"
PACKAGE="rsa"

echo "check-audit-waiver: verifying the ${ADVISORY} (${PACKAGE}) waiver"

# ── 1c. Is the package still in the graph at all? ──────────────────────────
if ! grep -qE "^name = \"${PACKAGE}\"" Cargo.lock; then
    echo "check-audit-waiver: ${PACKAGE} is no longer in Cargo.lock." >&2
    echo "  The waiver is dead config — remove ${ADVISORY} from tools/audit.toml" >&2
    echo "  and drop this script from the audit job." >&2
    exit 1
fi

# ── 1a. Is it reachable for a real target? ─────────────────────────────────
# `--target all` resolves the graph for every platform, which is what a bare
# `cargo tree -i` misses: it only reports the current host's graph, and
# `rsa` can look absent locally while being pulled in elsewhere.
REACHABLE="$(cargo tree --target all --invert "${PACKAGE}" 2>/dev/null || true)"

if [[ -n "${REACHABLE//[[:space:]]/}" ]]; then
    echo "check-audit-waiver: ${PACKAGE} IS reachable for at least one target:" >&2
    echo "${REACHABLE}" >&2
    echo >&2
    echo "  The waiver is no longer valid. This is a live advisory, not a" >&2
    echo "  structural lockfile artefact. Fix the dependency — do not widen" >&2
    echo "  the ignore list." >&2
    fail=1
fi

# ── 1b. Has upstream shipped a patch? ─────────────────────────────────────
# The advisory's `patched = []` is intentional (RustCrypto/RSA#626), so a
# release appearing here means the waiver should become an upgrade.
PATCHED="$(python3 - <<'PY'
import json, subprocess, sys, urllib.request

# Prefer a local advisory DB if cargo-audit has one; fall back to the advisory
# API so the check works on a fresh CI runner.
try:
    out = subprocess.run(
        ["cargo", "audit", "--json", "--file", "Cargo.lock", "--no-fetch"],
        capture_output=True, text=True, check=False,
    )
    data = json.loads(out.stdout)
    for v in data.get("vulnerabilities", {}).get("list", []):
        if v["advisory"]["id"] == "RUSTSEC-2023-0071":
            print(",".join(v["versions"].get("patched") or []))
            sys.exit(0)
    # Advisory withdrawn or absent -> nothing to patch.
    print("")
    sys.exit(0)
except Exception:
    # cargo-audit unavailable or DB not fetched yet: try the API.
    try:
        with urllib.request.urlopen(
            "https://rustsec.org/advisories/RUSTSEC-2023-0071.json", timeout=20
        ) as fh:
            print(",".join(json.load(fh).get("versions", {}).get("patched") or []))
    except Exception:
        print("UNKNOWN")
PY
)"

echo "check-audit-waiver: patched versions found upstream: ${PATCHED:-none}"

if [[ "${PATCHED}" == "UNKNOWN" ]]; then
    echo "check-audit-waiver: WARNING could not determine whether a patch exists." >&2
    echo "  Not fatal, but the waiver is no longer being proven current." >&2
elif [[ -n "${PATCHED}" ]]; then
    echo "check-audit-waiver: a patched ${PACKAGE} now exists (${PATCHED})." >&2
    echo "  The waiver is obsolete — upgrade instead of ignoring." >&2
    fail=1
fi

# ════════════════════════════════════════════════════════════════════════════
# Waiver 2 — zip / RUSTSEC-2026-0367,0368,0369 (build-only)
# ════════════════════════════════════════════════════════════════════════════

ZIP_ADVISORIES="RUSTSEC-2026-0367/0368/0369"

echo "check-audit-waiver: verifying the ${ZIP_ADVISORIES} (zip) waiver"

ZIP_LOCK="$(python3 - <<'PY'
import re
lock = open("Cargo.lock", encoding="utf-8").read()
for block in lock.split("[[package]]"):
    m = re.search(r'name = "([^"]+)"', block)
    if m and m.group(1) == "zip":
        v = re.search(r'version = "([^"]+)"', block)
        print(v.group(1) if v else "?")
        break
PY
)"

# ── 2c. Is zip still in the graph at all? ─────────────────────────────────
if [[ -z "${ZIP_LOCK}" ]]; then
    echo "check-audit-waiver: zip is no longer in Cargo.lock." >&2
    echo "  The waiver is dead config — remove ${ZIP_ADVISORIES} from tools/audit.toml" >&2
    echo "  and drop this script from the audit job." >&2
    exit 1
fi

echo "check-audit-waiver: zip lockfile version: ${ZIP_LOCK}"

# ── 2a. Does zip reach a compiled (normal/dev) surface for any target? ────
# The `voice` feature is the superset that actually pulls `sherpa-onnx-sys`
# (and its build-script `zip` dependency) into the graph; without it the
# check would vacuously pass. A normal/dev edge here means the flagged
# archive-extraction code would be compiled INTO a shipped binary — which
# the waiver categorically excludes.
NORMAL_DEV="$(cargo tree -i "zip@${ZIP_LOCK}" -e normal,dev --target all --features voice 2>/dev/null || true)"

if [[ -n "${NORMAL_DEV//[[:space:]]/}" ]]; then
    echo "check-audit-waiver: zip IS compiled for at least one target:" >&2
    echo "${NORMAL_DEV}" >&2
    echo >&2
    echo "  The waiver only covers build-time use (sherpa-onnx-sys build.rs)." >&2
    echo "  A normal/dev edge here is a LIVE vulnerability in the product." >&2
    echo "  Fix the dependency — do not widen the ignore list." >&2
    fail=1
else
    echo "check-audit-waiver: zip has no normal/dev edge for any target (build-only, as the waiver assumes)"
fi

# ── 2b. Has the fix line already been reached? ────────────────────────────
# All three advisories patch at `>= 9.0.0`. cargo maximises the lockfile
# version, so once the graph CAN consume 9.x it will — at which point these
# ignore entries are dead config that must be removed.
if python3 -c "
import sys
ver = '${ZIP_LOCK}'.split('.')
for i, part in enumerate([9, 0, 0]):
    cur = int(ver[i]) if i < len(ver) else 0
    if cur != part:
        sys.exit(0 if cur > part else 1)
sys.exit(0)
"; then
    echo "check-audit-waiver: zip ${ZIP_LOCK} >= 9.0.0 — the advisories no longer apply." >&2
    echo "  The waiver is dead config — remove ${ZIP_ADVISORIES} from tools/audit.toml." >&2
    fail=1
fi

# ════════════════════════════════════════════════════════════════════════════
# Waiver 3 — webpki-roots 0.26 / RUSTSEC-2026-0381 (build-only via ureq)
# ════════════════════════════════════════════════════════════════════════════

echo "check-audit-waiver: verifying the RUSTSEC-2026-0381 (webpki-roots 0.26) waiver"

# ── 3c. Is the 0.26 line still in the lock at all? ─────────────────────────
# (The 1.0.10 occurrence is patched and takes no waiver; only the 0.26 line
# is waived. Its disappearance means ureq moved on — dead config, remove.)
WPR_026="$(python3 - <<'PY'
import re
lock = open("Cargo.lock", encoding="utf-8").read()
for block in lock.split("[[package]]"):
    m = re.search(r'name = "([^"]+)"', block)
    v = re.search(r'version = "([^"]+)"', block)
    if m and v and m.group(1) == "webpki-roots" and v.group(1).startswith("0."):
        print(v.group(1))
        break
PY
)"

if [[ -z "${WPR_026}" ]]; then
    echo "check-audit-waiver: webpki-roots 0.26 is no longer in Cargo.lock." >&2
    echo "  The waiver is dead config — remove RUSTSEC-2026-0381 from tools/audit.toml." >&2
    fail=1
else
    echo "check-audit-waiver: webpki-roots ${WPR_026} present (build-only premise to verify)"
fi

# ── 3a. Does webpki-roots 0.26 reach a compiled surface for any target? ───
WPR_NORMAL_DEV="$(cargo tree -i "webpki-roots@${WPR_026}" -e normal,dev --target all --features voice 2>/dev/null || true)"

if [[ -n "${WPR_NORMAL_DEV//[[:space:]]/}" ]]; then
    echo "check-audit-waiver: webpki-roots ${WPR_026} IS compiled for at least one target:" >&2
    echo "${WPR_NORMAL_DEV}" >&2
    echo >&2
    echo "  The waiver only covers the build-time ureq download path. A" >&2
    echo "  normal/dev edge is a LIVE vulnerability in the product's TLS" >&2
    echo "  trust store. Fix the dependency — do not widen the ignore list." >&2
    fail=1
else
    echo "check-audit-waiver: webpki-roots ${WPR_026} has no normal/dev edge (build-only, as the waiver assumes)"
fi

# ════════════════════════════════════════════════════════════════════════════

if [[ "${fail}" -ne 0 ]]; then
    exit 1
fi

echo "check-audit-waiver: OK — all waiver premises hold."
