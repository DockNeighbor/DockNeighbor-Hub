#!/bin/sh
# Strip whole-line comments and blank lines from a shell script, for the PACKAGED copy only.
# Usage: sh strip-comments.sh <source> <destination>
#
# WHY. The router has 416 KB of free overlay and roughly half of brvg-hub-lite.sh is commentary —
# the reasoning that keeps the next person from re-breaking something, which belongs in git and
# nowhere near a flash chip. The source is never touched: only build-ipk.sh's payload is stripped.
#
# DELIBERATELY DUMB, because a clever stripper is how a script gets broken on a router:
#   * only lines whose FIRST non-blank character is `#` go, plus empty lines. A trailing `# ...` is
#     left alone: `$#`, `${x#y}` and `#` inside a quoted string all look like comments to a regex.
#   * the shebang on line 1 stays.
#   * a heredoc body is copied VERBATIM, `#` lines and blank lines included — a heredoc is data.
# test.sh strips every packaged script with this, checks each with `sh -n`, and runs the whole suite
# against the stripped copies, so a construct this cannot handle fails CI rather than a boat.
set -eu
[ $# -eq 2 ] || { echo "usage: $0 <source> <destination>" >&2; exit 2; }
awk '
  heredoc != "" {
    print
    line = $0
    if (strip_tabs) sub(/^\t+/, "", line)
    if (line == heredoc) heredoc = ""
    next
  }
  NR == 1 && /^#!/ { print; next }
  /^[[:space:]]*#/ { next }
  /^[[:space:]]*$/ { next }
  {
    print
    if (match($0, /<<-?[[:space:]]*["\047]?[A-Za-z_][A-Za-z0-9_]*["\047]?/)) {
      tag = substr($0, RSTART, RLENGTH)
      strip_tabs = (tag ~ /^<<-/)
      sub(/^<<-?[[:space:]]*/, "", tag); gsub(/["\047]/, "", tag)
      heredoc = tag
    }
  }' "$1" > "$2"
# Keep the source's mode, so an executable stays executable.
[ -x "$1" ] && chmod +x "$2"
exit 0
