#!/bin/sh
# Build an OpenWrt .ipk of the phone-home hub-lite.
#
# WHY this exists (ONSITE.md "Router phone-home"): the app installs the hub-lite over SSH today, which works
# but is desktop-only — the SSH client is deliberately kept out of the Android build. GL.iNet's 4.x
# RPC surface has `plugins.install_package`, so a package installs with **no SSH at all**, from any
# platform including a phone. It is also the natural home for the signed-artifact bar: opkg feeds
# are signed (usign), so the signature check happens on-device, by the package manager, rather than
# being something we invent.
#
# This script produces the package. SIGNING and PUBLISHING it — the signed opkg feed a router's
# self_update pulls from — is done by .github/workflows/hub-lite-feed.yml on a hub-lite-v* tag; see
# hub-lite/package/README.md for the feed URL and the one owner step (the signing secret).
#
# ipk format: a GZIPPED TAR of debian-binary + control.tar.gz + data.tar.gz (NOT an ar archive —
# that is .deb; see the sealing block below). No OpenWrt SDK, no cross-compiler: the hub-lite is
# POSIX shell, so the "build" is packaging.

set -eu

SRC="$(cd "$(dirname "$0")/.." && pwd)"
# The version comes from the hub-lite itself. Two sources of truth would mean the feed advertising a
# version the running hub-lite doesn't report — which is precisely the signal a staged rollout uses
# to decide who still needs updating.
VERSION="${VERSION:-$(sed -n 's/^HUB_LITE_VERSION="\([^"]*\)".*/\1/p' "$SRC/brvg-hub-lite.sh")}"
[ -n "$VERSION" ] || { echo "could not read HUB_LITE_VERSION from brvg-hub-lite.sh" >&2; exit 1; }
OUT="${OUT:-$(pwd)/dist}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PKG="brvg-hub-lite"
ARCH="all"                 # pure shell — architecture-independent

mkdir -p "$WORK/data/usr/bin" "$WORK/data/usr/libexec/brvg-hub-lite" "$WORK/data/etc/init.d" "$WORK/data/www/brvg/cgi-bin" "$WORK/data/www/brvg/api" "$WORK/control" "$OUT"

# 🔬 THE PAYLOAD IS COMMENT-STRIPPED; THE SOURCE IS NOT. Roughly half of these scripts is the
# reasoning that stops the next person re-breaking them, which belongs in git and not on a router
# with 416 KB of free overlay (GL-X750) — or a GL-MT300N-V2 with 16 MB of flash in total. Whole-line
# comments and blank lines only; see strip-comments.sh for why nothing cleverer is attempted, and
# test.sh for the gate (every stripped script passes `sh -n` and the full suite runs against them).
#
# setup-usb-gps is NOT stripped: its --help prints its own header comment.
STRIP="$SRC/package/strip-comments.sh"
ship() {  # $1 source, $2 destination, $3 mode
  sh "$STRIP" "$1" "$2"
  chmod "$3" "$2"
}
_src_bytes=0; _ship_bytes=0
for _f in brvg-hub-lite.sh hub-lite-api.sh hub-lite-cgi.sh hub-lite-mgmt.sh package/feed-setup.sh openwrt/etc/init.d/brvg-hub-lite; do
  _src_bytes=$(( _src_bytes + $(wc -c < "$SRC/$_f") ))
done

ship "$SRC/brvg-hub-lite.sh" "$WORK/data/usr/bin/brvg-hub-lite" 0755
# The signed-feed provisioner: writes the trust anchor and the customfeeds line self_update needs.
# Shipped as a payload file (not just baked into postinst) so a re-run — or the app's over-SSH
# installer, which embeds the same content — has one canonical script to call.
ship "$SRC/package/feed-setup.sh" "$WORK/data/usr/libexec/brvg-hub-lite/feed-setup" 0755
ship "$SRC/openwrt/etc/init.d/brvg-hub-lite" "$WORK/data/etc/init.d/brvg-hub-lite" 0755
# USB GPS → TCP NMEA setup. SHIPPED but not run at install: the dongle is normally plugged in later,
# and it needs working internet for opkg. `brvg-setup-usb-gps` on the router does the whole job.
install -m 0755 "$SRC/setup-usb-gps.sh" "$WORK/data/usr/bin/brvg-setup-usb-gps"
# Relay tier: the CGI webhook receiver (inert until HUB_LITE_ENABLED=1 in the config).
ship "$SRC/hub-lite-cgi.sh" "$WORK/data/www/brvg/cgi-bin/report" 0755
ship "$SRC/hub-lite-mgmt.sh" "$WORK/data/www/brvg/cgi-bin/mgmt" 0755
# The /api/hub/* door. Installed WITHOUT a .sh suffix and directly at api/hub, because uhttpd
# resolves the longest existing file path and hands the rest over as PATH_INFO — so this one file
# answers /api/hub/ping, /api/hub/status and /api/hub/linktap/state.
ship "$SRC/hub-lite-api.sh" "$WORK/data/www/brvg/api/hub" 0755

for _f in usr/bin/brvg-hub-lite www/brvg/api/hub www/brvg/cgi-bin/report www/brvg/cgi-bin/mgmt usr/libexec/brvg-hub-lite/feed-setup etc/init.d/brvg-hub-lite; do
  _ship_bytes=$(( _ship_bytes + $(wc -c < "$WORK/data/$_f") ))
  # A stripped script that no longer parses must never be sealed into a package.
  sh -n "$WORK/data/$_f" || { echo "stripped $_f does not parse" >&2; exit 1; }
done
echo "scripts: ${_src_bytes} B source -> ${_ship_bytes} B shipped (comments stripped)"

# NOTE: no config file ships in the package. The config carries this device's token and is written
# by the app at enrollment; packaging a placeholder would risk overwriting a live one on upgrade.
# uhttpd serves the hub-lite CGI receiver and is NOT stock on GL.iNet firmware (bench GL-X750
# 4.3.28 ships nginx only) — opkg pulls it at install, which needs router internet.
cat > "$WORK/control/control" <<EOF
Package: $PKG
Version: $VERSION
Depends: libc, curl, uhttpd
Section: net
Architecture: $ARCH
Maintainer: DockNeighbor
Description: Reports GPS and modem telemetry to DockNeighbor, so the vehicle
 keeps reporting when nobody has the app open aboard.
EOF

# Keep an existing /etc/brvg-hub-lite.conf across upgrades — it holds the device's credential.
cat > "$WORK/control/conffiles" <<'EOF'
/etc/brvg-hub-lite.conf
EOF

cat > "$WORK/control/postinst" <<'EOF'
#!/bin/sh
[ -f /etc/brvg-hub-lite.conf ] && chmod 600 /etc/brvg-hub-lite.conf
/etc/init.d/brvg-hub-lite enable 2>/dev/null || true
# Point opkg at the signed feed so the NEXT self_update has somewhere to pull from. Best-effort:
# feed-setup runs `set -e`, but a router with a read-only or unusual /etc must still finish
# installing the hub-lite, so a failure here is logged and swallowed rather than failing the
# package.
[ -x /usr/libexec/brvg-hub-lite/feed-setup ] && { /usr/libexec/brvg-hub-lite/feed-setup || echo "BRVG_FEED_SETUP_SKIPPED"; }
# 🔴 AN UPGRADE MUST NOT LEAVE A ROUTER WITH ITS COLLECTOR STOPPED.
#
# prerm stops and disables the service, so on an `opkg upgrade` the sequence is stop -> unpack ->
# postinst, and this script used to end without ever starting it again. Measured on sc4-lab
# 2026-09-03: a clean feed upgrade 0.14.5 -> 0.14.6 succeeded in every other respect and left
# hub-lite NOT RUNNING. It would have come back at the next reboot, which on a boat could be weeks
# of silence, reported by nothing.
#
# 0.15.0: STARTED ON A FRESH INSTALL TOO. The reason it used to wait for a config was real — with no
# VID the collector exited on a fatal error and procd respawned it forever. The collector now WAITS
# for a vehicle instead (main() in brvg-hub-lite.sh), and starting the service is what brings up the
# uhttpd door whose /api/hub/identity + /api/hub/bootstrap let the app set the router up with no
# SSH at all — inside the 15-minute window that this start opens.
/etc/init.d/brvg-hub-lite start 2>/dev/null || echo "BRVG_START_FAILED"
exit 0
EOF
chmod 0755 "$WORK/control/postinst"

cat > "$WORK/control/prerm" <<'EOF'
#!/bin/sh
/etc/init.d/brvg-hub-lite stop 2>/dev/null || true
/etc/init.d/brvg-hub-lite disable 2>/dev/null || true
exit 0
EOF
chmod 0755 "$WORK/control/prerm"

echo "2.0" > "$WORK/debian-binary"

# ⚠️ EVERY TAR HERE IS ustar, AND ON macOS AppleDouble IS SUPPRESSED. busybox tar — which is what
# opkg uses on the router — cannot read pax extended headers, and macOS `tar` writes them BY
# DEFAULT. A package built on a Mac otherwise installs as a pile of `PaxHeader/...` and `._...`
# entries with `get_header_tar: Unknown typeflag: 0x78`, having unpacked none of the real files.
# Measured on sc4-lab, 2026-09-03. CI builds on Ubuntu and would not have shown this, which is
# exactly why the flag belongs in the script rather than in the runner.
TAR_OPTS="--format=ustar"
export COPYFILE_DISABLE=1
( cd "$WORK/data" && tar $TAR_OPTS -czf "$WORK/data.tar.gz" . )
( cd "$WORK/control" && tar $TAR_OPTS -czf "$WORK/control.tar.gz" . )

# Verify the payload before sealing it: an empty or short package installs "successfully" and
# leaves the router with no hub-lite, which is the failure mode worth catching here rather than on
# somebody's boat.
tar tzf "$WORK/data.tar.gz" | grep -q './usr/bin/brvg-hub-lite' || { echo "payload missing the hub-lite" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './usr/libexec/brvg-hub-lite/feed-setup' || { echo "payload missing the feed provisioner" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './etc/init.d/brvg-hub-lite' || { echo "payload missing the init script" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './www/brvg/cgi-bin/report' || { echo "payload missing the relay CGI" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './www/brvg/cgi-bin/mgmt' || { echo "payload missing the management CGI" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './www/brvg/api/hub' || { echo "payload missing the /api/hub CGI" >&2; exit 1; }
tar tzf "$WORK/control.tar.gz" | grep -q './control' || { echo "control archive incomplete" >&2; exit 1; }

IPK="$OUT/${PKG}_${VERSION}_${ARCH}.ipk"
rm -f "$IPK"

# 🔴 AN OPENWRT .ipk IS A GZIPPED TAR, NOT AN `ar` ARCHIVE. That distinction is the whole of this
# block, and getting it wrong is why no router has ever installed this package from the feed.
#
# .deb is ar. .ipk is tar.gz. This script built an ar for its entire life — the comment here even
# asserted "ipk format: an ar archive", confidently and wrongly — and opkg's answer to it is
# `pkg_init_from_file: Malformed package file`. Measured against a real OpenWrt package on
# 2026-09-03: rpcd_..._mips_24kc.ipk begins `1f 8b` (gzip) and unpacks to ./debian-binary,
# ./data.tar.gz, ./control.tar.gz. Ours began `!<arch>`.
#
# ⚠️ THE FAILURE WAS WORSE THAN AN ERROR, WHICH IS WHY IT SURVIVED SO LONG. Installing the .ipk
# BY PATH says "Malformed package file" and stops. Installing the same package FROM A FEED prints
# "Installing… Configuring… Database update completed", exits 0, registers the package, writes a
# ZERO-BYTE file list and unpacks NOTHING. A router is then "running 0.14.5" by `opkg list-installed`
# while every file on disk is whatever was there before. Both known routers were installed by hand
# copy, so nobody ever hit the honest error.
#
# Still no toolchain: tar and gzip only, exactly as before.
( cd "$WORK" && tar $TAR_OPTS -czf "$IPK" ./debian-binary ./control.tar.gz ./data.tar.gz )

# Sanity-check the sealed archive. The old check asserted the WRONG format and passed every time —
# a gate that confirms a mistake is worse than no gate, so this one verifies what opkg actually
# needs: gzip magic, and all three members present under the ./ prefix a real .ipk uses.
[ "$(dd if="$IPK" bs=1 count=2 2>/dev/null | od -An -tx1 | tr -d ' \n')" = "1f8b" ] \
  || { echo "not a gzip — an .ipk is a tar.gz, not an ar archive" >&2; exit 1; }
for _m in ./debian-binary ./control.tar.gz ./data.tar.gz; do
  tar tzf "$IPK" | grep -qx "$_m" || { echo "sealed package is missing $_m" >&2; exit 1; }
done
# busybox tar chokes on pax headers and AppleDouble; catch them here, not on a router.
for _a in "$IPK" "$WORK/data.tar.gz" "$WORK/control.tar.gz"; do
  tar tzf "$_a" | grep -qE 'PaxHeader|/\._|^\./\._' && { echo "archive $_a carries pax/AppleDouble entries busybox tar cannot read" >&2; exit 1; }
done
[ "$(wc -c < "$IPK" | tr -d ' ')" -gt 2000 ] || { echo "package suspiciously small" >&2; exit 1; }

echo "built $IPK ($(wc -c < "$IPK" | tr -d ' ') B)"
