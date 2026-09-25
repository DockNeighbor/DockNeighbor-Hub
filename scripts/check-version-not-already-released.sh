#!/bin/sh
# Does this branch declare a version that has ALREADY BEEN PUBLISHED by somebody else?
#
# WHY THIS EXISTS (2026-09-25)
# ---------------------------
# 🔴 A NEAR-MISS, CAUGHT BY HAND. PR #168 was held for an owner decision while three separate PRs
# released hub-lite 0.18.3, 0.18.4 and 0.18.5 underneath it. Each time, main and the branch ended up
# declaring the SAME version string — so `git merge` took it with NO conflict, CI went green, all 867
# hub-lite checks passed, and the branch sat one `git tag` away from publishing different code under a
# version already installed on routers.
#
# Nothing in the pipeline compared a branch's declared version against the published tags. The release
# command itself would have been the first thing to notice, by failing — or, worse, by not failing and
# moving the tag. It was found only because somebody ran `git tag -l` before releasing, out of habit.
# Habits are not checks.
#
# WHAT IT CHECKS
# --------------
# For each shipped artifact's declared version, is there already a tag of that name — and if so, is it
# OURS?
#
#   hub-lite/brvg-hub-lite.sh  HUB_LITE_VERSION  ->  hub-lite-v<V>
#   daemon/Cargo.toml          [package] version ->  daemon-v<V>
#
# ⚠️ "THE TAG EXISTS" IS NOT ENOUGH TO FAIL, and getting that wrong would make this check useless in the
# other direction. Immediately after a release, main legitimately declares the version it just tagged:
# the tag exists and points into our own history. So the rule is REACHABILITY, not existence:
#
#   tag absent                        -> fine, nothing published yet
#   tag reachable from HEAD           -> fine, that release is ours
#   tag exists, NOT reachable         -> FAIL. Somebody else published this version; bump.
#
# That is exactly the distinction that separated "#168 is about to overwrite #170's release" from
# "main is sitting on the release it just cut".
#
# EXIT CODES — three branches, on purpose
#   0  every declared version is unpublished, or published by this history
#   1  a version is already published elsewhere -> bump the version
#   2  could not derive a version or could not see the tags. NOT a pass: a version this cannot parse,
#      or a shallow clone with no tags fetched, is a check that did not run.
#
# USAGE
#   sh scripts/check-version-not-already-released.sh [repo-root]
# In CI the tags MUST be present — actions/checkout needs `fetch-depth: 0` and `fetch-tags: true`, or
# this exits 2 rather than pretending.

set -eu

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
SH_FILE="$ROOT/hub-lite/brvg-hub-lite.sh"
TOML_FILE="$ROOT/daemon/Cargo.toml"
problems=0

unverifiable() {
  echo "🟡 UNVERIFIABLE: $1" >&2
  echo "   A check that cannot run must not report success." >&2
  exit 2
}

[ -f "$SH_FILE" ] || unverifiable "no $SH_FILE — wrong root, or the file was renamed"
[ -f "$TOML_FILE" ] || unverifiable "no $TOML_FILE — wrong root, or the file was renamed"

# A repository with no tags at all is almost always a shallow CI checkout, not a repository that has
# never released. Treating that as "nothing is published" is the exact false pass this exists to stop.
if [ -z "$(git -C "$ROOT" tag -l 2>/dev/null | head -n 1)" ]; then
  unverifiable "this checkout has NO tags — a shallow clone cannot see what is published (actions/checkout needs fetch-depth: 0 and fetch-tags: true)"
fi

# `HUB_LITE_VERSION="0.18.7"` — the assignment, not a mention in prose.
hub_lite_version=$(sed -n 's/^HUB_LITE_VERSION="\([^"]*\)".*/\1/p' "$SH_FILE" | head -n 1)
# The FIRST `version = "x"` after `[package]`, so a dependency's version is never read as ours.
daemon_version=$(awk '
  /^\[package\]/ { inpkg = 1; next }
  /^\[/          { inpkg = 0 }
  inpkg && /^version[[:space:]]*=/ {
    gsub(/^version[[:space:]]*=[[:space:]]*"/, ""); gsub(/".*/, ""); print; exit
  }' "$TOML_FILE")

check_one() {
  _name="$1"; _version="$2"; _tag_prefix="$3"
  case "$_version" in
    '' ) unverifiable "could not parse $_name's version — the declaration moved or changed shape" ;;
    *[!0-9.]* ) unverifiable "$_name's version is not a plain dotted number: '$_version'" ;;
  esac
  _tag="${_tag_prefix}${_version}"
  if ! git -C "$ROOT" rev-parse -q --verify "refs/tags/$_tag" >/dev/null 2>&1; then
    echo "  ok   $_name $_version — $_tag is not published yet"
    return 0
  fi
  # The tag exists. Ours, or somebody else's?
  if git -C "$ROOT" merge-base --is-ancestor "$_tag" HEAD 2>/dev/null; then
    echo "  ok   $_name $_version — $_tag exists and is in this history (our own release)"
    return 0
  fi
  echo "  FAIL $_name $_version — $_tag is ALREADY PUBLISHED, at $(git -C "$ROOT" rev-parse --short "$_tag"), which is NOT in this history."
  echo "       Somebody released $_version while this branch was open. Publishing would put different"
  echo "       code under a version that is already installed in the field. Bump $_name and re-run."
  problems=$((problems + 1))
}

echo "declared versions vs published tags — root $ROOT"
check_one "hub-lite" "$hub_lite_version" "hub-lite-v"
check_one "daemon" "$daemon_version" "daemon-v"

if [ "$problems" -gt 0 ]; then
  echo ""
  echo "$problems version(s) already published. This is the failure git does not report: two branches"
  echo "declaring the same version merge with no conflict, and every test still passes."
  exit 1
fi
exit 0
