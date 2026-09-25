#!/bin/sh
# Selftest for check-version-not-already-released.sh — does it actually FAIL when it should?
#
# The gate is worth exactly what its red states are worth, so every branch is exercised against a
# throwaway repository built here: unpublished, published-by-us, published-by-someone-else, and the
# three ways it can fail to evaluate. The centre case is the real 2026-09-25 near-miss, reproduced:
# a branch declaring a version whose tag exists on a commit that is NOT in its history.
#
# It also asserts the CLEAN cases. A gate that always fails is as useless as one that always passes,
# and the reachability rule is exactly where a too-eager version of this would break every release.
#
# USAGE  sh scripts/check-version-not-already-released.selftest.sh
# Exit 0 = the gate behaves. 1 = a case did not behave.

set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GATE="$HERE/check-version-not-already-released.sh"
[ -f "$GATE" ] || { echo "🟡 UNVERIFIABLE: no $GATE"; exit 2; }

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
fails=0

# One throwaway repo, shaped like the real one: the two declaration files and a history we can tag.
mk() {  # mk <hub-lite version> <daemon version>
  rm -rf "$W/r"; mkdir -p "$W/r/hub-lite" "$W/r/daemon" "$W/r/scripts"
  git -C "$W/r" init -q
  git -C "$W/r" config user.email t@t; git -C "$W/r" config user.name t
  printf '#!/bin/sh\nHUB_LITE_VERSION="%s"\n' "$1" > "$W/r/hub-lite/brvg-hub-lite.sh"
  printf '[package]\nname = "brvg-hub"\nversion = "%s"\n\n[dependencies]\nserde = "1.0.99"\n' "$2" > "$W/r/daemon/Cargo.toml"
  git -C "$W/r" add -A; git -C "$W/r" commit -qm base
}
set_hub_lite() { printf '#!/bin/sh\nHUB_LITE_VERSION="%s"\n' "$1" > "$W/r/hub-lite/brvg-hub-lite.sh"; }

# ⚠️ `|| _rc=$?` IS LOAD-BEARING UNDER `set -e`. Written as a bare call, the FIRST case where the
# gate legitimately exits non-zero aborts this subshell before it can report the code — so the selftest
# died silently at its first real finding and reported "1 case failed" with no output. The whole point of
# this file is to observe non-zero exits, so it must not treat one as its own error.
run() { _rc=0; sh "$GATE" "$W/r" >"$W/out" 2>&1 || _rc=$?; echo "$_rc"; }

check() {  # check <name> <expected exit> [expected text]
  _name="$1"; _want="$2"; _text="${3:-}"
  _got=$(run)
  if [ "$_got" = "$_want" ] && { [ -z "$_text" ] || grep -q "$_text" "$W/out"; }; then
    echo "  ok   $_name -> exit $_got"
  else
    echo "  FAIL $_name -> exit $_got (wanted $_want${_text:+ containing '$_text'})"
    sed 's/^/         /' "$W/out"
    fails=$((fails + 1))
  fi
}

echo "check-version-not-already-released selftest"

# ── exit 0: nothing published, or published by us ────────────────────────────────────────────────
mk 0.18.7 0.3.57
git -C "$W/r" tag hub-lite-v0.18.6
git -C "$W/r" tag daemon-v0.3.56
check "neither declared version is tagged yet" 0 "not published yet"

# The case a naive "does the tag exist" check would break: main right after cutting its own release.
mk 0.18.8 0.3.58
git -C "$W/r" tag hub-lite-v0.18.8
git -C "$W/r" tag daemon-v0.3.58
check "the tag exists and is OUR OWN release (reachable from HEAD)" 0 "our own release"

# ── exit 1: somebody else published this version ─────────────────────────────────────────────────
# 🔴 THE REAL 2026-09-25 NEAR-MISS. A sibling branch releases 0.18.9 and tags it; our branch, which
# does not contain that commit, still declares 0.18.9. Git merges the identical string with no
# conflict and every test passes — this is the only thing that objects.
mk 0.18.8 0.3.59
_main=$(git -C "$W/r" rev-parse --abbrev-ref HEAD)
git -C "$W/r" checkout -q -b sibling
set_hub_lite 0.18.9
git -C "$W/r" commit -qam "sibling releases 0.18.9"
git -C "$W/r" tag hub-lite-v0.18.9
# Back on our own branch, which does NOT contain the sibling's commit, and declare the same version —
# the exact state #168 reached after merging main took the identical string with no conflict.
git -C "$W/r" checkout -q "$_main"
set_hub_lite 0.18.9
git -C "$W/r" commit -qam "our branch happens to declare 0.18.9 too"
check "hub-lite version published on a commit NOT in our history" 1 "ALREADY PUBLISHED"

mk 0.19.0 0.3.60
git -C "$W/r" checkout -q -b sibling2
git -C "$W/r" commit -q --allow-empty -m "sibling releases the daemon"
git -C "$W/r" tag daemon-v0.3.60
git -C "$W/r" checkout -q master 2>/dev/null || git -C "$W/r" checkout -q main
check "daemon version published elsewhere is caught too" 1 "daemon 0.3.60"

# ── exit 2: it could not evaluate, and must NOT pass ─────────────────────────────────────────────
mk 0.18.7 0.3.57
git -C "$W/r" tag hub-lite-v0.18.6
sed -i.bak 's/^HUB_LITE_VERSION=.*/HUB_LITE_VERSION_RENAMED="0.18.7"/' "$W/r/hub-lite/brvg-hub-lite.sh"
check "the hub-lite declaration is renamed" 2 "could not parse"

mk 0.18.7 0.3.57
git -C "$W/r" tag hub-lite-v0.18.6
printf '[package]\nname = "brvg-hub"\n\n[dependencies]\nserde = "1.0.99"\n' > "$W/r/daemon/Cargo.toml"
# ⚠️ AND NOT "1.0.99". Without the [package]-section guard this would read the dependency's version and
# confidently check daemon-v1.0.99 — a precise, wrong answer, which is worse than no answer.
check "the daemon version is missing (and a dependency is not mistaken for it)" 2 "could not parse"

mk 0.18.7 0.3.57
check "a checkout with NO tags cannot see what is published" 2 "NO tags"

mk 0.18.7 0.3.57
git -C "$W/r" tag hub-lite-v0.18.6
rm -f "$W/r/hub-lite/brvg-hub-lite.sh"
check "a declaration file is gone" 2 "no "

mk 0.18.7-dirty 0.3.57
git -C "$W/r" tag hub-lite-v0.18.6
check "a version that is not a plain dotted number" 2 "not a plain dotted number"

if [ "$fails" -gt 0 ]; then
  echo ""
  echo "$fails case(s) did not behave. A check you have not watched fail is not evidence."
  exit 1
fi
echo ""
echo "OK — the gate fails for the right reasons, and passes on a release it cut itself."
exit 0
