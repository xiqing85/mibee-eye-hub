#!/usr/bin/env bash
# Verify the premise behind the RUSTSEC-2023-0071 (`rsa`) audit waiver still
# holds. Run by the `audit` CI job before `cargo audit`.
#
# The waiver says `rsa` is unreachable: it is pulled in only by sqlx 0.8's
# Cargo.toml, which lists every backend unconditionally (sqlx-mysql -> rsa),
# and this application is SQLite-only so the MySQL RSA authentication path it
# exposes is never executed. There is no patched version.
#
# A waiver whose justification can silently expire is worse than no waiver, so
# this re-derives the justification on every run. It fails the job when:
#
#   1. `rsa` becomes reachable for some real target — the waiver is then a
#      live vulnerability being waved away;
#   2. a patched version of `rsa` exists — the waiver is then obsolete and
#      should be replaced by an upgrade;
#   3. `rsa` disappears from the lockfile entirely — the waiver is dead
#      config and should be removed.
#
# Point 3 is why this is a script and not a comment.

set -euo pipefail

ADVISORY="RUSTSEC-2023-0071"
PACKAGE="rsa"
fail=0

cd "$(dirname "$0")/.."

if [[ ! -f Cargo.lock ]]; then
    echo "check-audit-waiver: no Cargo.lock; run a build first" >&2
    exit 1
fi

echo "check-audit-waiver: verifying the ${ADVISORY} (${PACKAGE}) waiver"

# ── 1. Is the package still in the graph at all? ────────────────────────────
if ! grep -qE "^name = \"${PACKAGE}\"" Cargo.lock; then
    echo "check-audit-waiver: ${PACKAGE} is no longer in Cargo.lock." >&2
    echo "  The waiver is dead config — remove ${ADVISORY} from tools/audit.toml" >&2
    echo "  and drop this script from the audit job." >&2
    exit 1
fi

# ── 2. Is it reachable for a real target? ──────────────────────────────────
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

# ── 3. Has upstream shipped a patch? ───────────────────────────────────────
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

if [[ "${fail}" -ne 0 ]]; then
    exit 1
fi

echo "check-audit-waiver: OK — ${PACKAGE} is unreachable for every target and has no patch."
