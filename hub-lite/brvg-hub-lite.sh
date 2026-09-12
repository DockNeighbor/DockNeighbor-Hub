#!/bin/sh
# BRVG phone-home hub-lite — Phase A skeleton (telemetry push; the command channel is Phase B).
#
# One POSIX-shell hub-lite, two homes: a GL.iNet router (busybox ash, AT commands straight to the
# modem port — the hub-lite runs as root on-device, so no RPC login is needed) and a Raspberry
# Pi-class hub (gpsd or a raw NMEA serial dongle). It pushes GPS and modem telemetry OUTBOUND
# over HTTPS to the hosted worker on a timer — no inbound path exists behind CGNAT, and none is
# needed. The command channel (Phase B) is deliberately absent from this skeleton.
#
# Auth: prefer the per-DEVICE revocable token (DEVICE_TOKEN → /api/agent, minted by
# /api/agent/enroll — worker increment 1), falling back to the per-vehicle webhook key
# (VEHICLE_KEY → /api/shelly) for configs written before tokens existed. A leaked token exposes
# one device's telemetry write path and is revoked individually; prefer it everywhere.
#
# Frugality: this rides the customer's own metered link. Cadences are configurable with floors,
# every request is a single small GET, and a failed send is dropped (next tick retries) rather
# than queued — Phase A is telemetry, not a store-and-forward system.
#
# Everything parseable is in awk functions at the top, exercised by hub-lite/test.sh with strings
# captured from real hardware (GL-X750 bench session 2026-08-06).
#
# Self-update (owner requirement: "build the secure solution"). The hub-lite NEVER downloads or
# evaluates code it was handed. `self_update` takes NO argument: it asks opkg to install from the
# SIGNED feed the router is already configured with, so WHAT gets installed is decided by the
# signed index and verified on-device by the package manager, while the cloud only decides WHO is
# told to update and WHEN (staged rollout). The previous hub-lite is kept and automatically restored
# if the new one cannot even report its own version.

HUB_LITE_VERSION="0.14.8"
HUB_LITE_BACKUP="/etc/brvg-hub-lite.prev"

# The LAST telemetry this hub-lite composed, as JSON, for the LAN management door to serve
# (hub-lite-mgmt.sh). Written by the same code that reports to the cloud, so the two can never
# disagree — and read rather than re-collected, so a status call never touches the AT port while
# the main loop is mid-read. tmpfs: it is a cache of something the cloud already has.
HUB_LITE_STATE="${BRVG_HUB_LITE_STATE:-/tmp/brvg-hub-lite.state}"

# The LAN management door runs verbs in a CGI process, NOT in this daemon. Anything a verb does to
# the filesystem, uci or the modem therefore lands for real — but FOLLOWUP_REPORT is a shell
# variable, and setting it in a process that immediately exits does nothing at all.
#
# ⚠️ THAT IS NOT THEORETICAL: on the bench (2026-08-21) `?action=command&cmd=report_now` returned
# 200 "ran" and the reported timestamp did not move for four minutes, because the whole effect of
# that verb IS the follow-up. So the CGI touches this file instead, and the loop below consumes it.
# One tick of latency, and honest — as against instant and false.
HUB_LITE_FOLLOWUP="${BRVG_HUB_LITE_FOLLOWUP:-/tmp/brvg-hub-lite.followup}"


# --- Pure parsers (stdin → stdout; empty output = no data) -------------------------------------

# AT+QGPSLOC=2 response → "lat lon acc" (acc = hdop*5, rough). Rejects 0/0 and out-of-range.
parse_qgpsloc() {
  awk -F'[:,]' '/\+QGPSLOC/ {
    lat = $3 + 0; lon = $4 + 0; hdop = $5 + 0
    if (lat == 0 && lon == 0) exit
    if (lat > 90 || lat < -90 || lon > 180 || lon < -180) exit
    # %.5f ≈ 1 m resolution; awk default %.6g would truncate 3-digit longitudes
    if (hdop > 0) printf "%.5f %.5f %.0f\n", lat, lon, hdop * 5
    else printf "%.5f %.5f\n", lat, lon
    exit
  }'
}

# AT+QCSQ response → "mode rssi rsrp sinr_db rsrq" (LTE only: raw sinr 0..250 → dB).
parse_qcsq() {
  awk '/\+QCSQ/ {
    line = $0; sub(/.*\+QCSQ:[ ]*/, "", line); gsub(/\r/, "", line)
    n = split(line, a, ",")
    gsub(/"/, "", a[1])
    if (a[1] == "LTE" && n >= 5) printf "%s %s %s %s %s\n", a[1], a[2], a[3], (a[4] / 5) - 20, a[5]
    else if (n >= 3) printf "%s %s %s\n", a[1], a[2], a[3]
    exit
  }'
}

# AT+COPS? response → carrier name (spaces preserved; caller URL-encodes).
parse_cops() {
  awk -F'"' '/\+COPS/ { print $2; exit }'
}

# AT+QGDCNT? response → "sent received" bytes since the counter was last reset.
parse_qgdcnt() {
  awk -F'[:,]' '/\+QGDCNT/ { gsub(/[^0-9]/, "", $2); gsub(/[^0-9]/, "", $3); if ($2 != "" && $3 != "") print $2, $3; exit }'
}

# AT+CPIN? response → ok | locked | missing
parse_cpin() {
  awk '/\+CPIN: READY/ { print "ok"; exit }
       /\+CPIN: SIM PIN|\+CPIN: SIM PUK/ { print "locked"; exit }
       /CME ERROR: 10|SIM not inserted/ { print "missing"; exit }'
}

# NMEA RMC sentence(s) → "lat lon" from the last valid fix (ddmm.mmmm → decimal degrees).
parse_nmea_rmc() {
  awk -F, '/RMC/ && $3 == "A" {
    v = $4 + 0; d = int(v / 100); lat = d + (v - d * 100) / 60; if ($5 == "S") lat = -lat
    v = $6 + 0; d = int(v / 100); lon = d + (v - d * 100) / 60; if ($7 == "W") lon = -lon
    if (lat == 0 && lon == 0) next
    # %.5f (≈1 m), matching the modem path — awk default OFMT is %.6g, which drops the USB dongle
    # to ~4 decimals (~11 m). Verified live on a u-blox 7 (bench 2026-08-13).
    out = sprintf("%.5f %.5f", lat, lon)
  } END { if (out != "") print out }'
}

# NCOS /api/status/gps → "lat lon". Bench shape (CBA850 fw 7.0.50, captured 2026-08-17): DMS
# objects with the SIGN riding on degree:
#   {"success":true,"data":{"fix":{"latitude":{"degree":41,"minute":29,"second":34.52},...}}}
# Same %.5f (≈1 m) as every other GPS parser here. A 0,0 placeholder is "no fix yet" → no output.
parse_cradlepoint_gps() {
  tr -d ' \n\t' | sed -n 's/.*"latitude":{"degree":\(-\{0,1\}[0-9.]*\),"minute":\([0-9.]*\),"second":\([0-9.]*\)}.*"longitude":{"degree":\(-\{0,1\}[0-9.]*\),"minute":\([0-9.]*\),"second":\([0-9.]*\)}.*/\1 \2 \3 \4 \5 \6/p' \
    | awk '{
        alat = ($1 < 0 ? -$1 : $1); lat = ($1 < 0 ? -1 : 1) * (alat + $2 / 60 + $3 / 3600)
        alon = ($4 < 0 ? -$4 : $4); lon = ($4 < 0 ? -1 : 1) * (alon + $5 / 60 + $6 / 3600)
        if (lat == 0 && lon == 0) exit
        printf "%.5f %.5f\n", lat, lon
      }'
}

# gpsd TPV JSON (gpspipe -w) → "lat lon [acc]" from the last 2D/3D fix.
parse_gpsd_tpv() {
  awk '/"class":"TPV"/ && /"mode":[23]/ {
    lat = ""; lon = ""; acc = ""
    if (match($0, /"lat":[-0-9.]+/))  lat = substr($0, RSTART + 6, RLENGTH - 6)
    if (match($0, /"lon":[-0-9.]+/))  lon = substr($0, RSTART + 6, RLENGTH - 6)
    if (match($0, /"eph":[0-9.]+/))   acc = substr($0, RSTART + 6, RLENGTH - 6)
    if (lat != "" && lon != "") out = lat " " lon (acc != "" ? " " acc : "")
  } END { if (out != "") print out }'
}

urlencode_spaces() { printf '%s' "$1" | sed 's/ /%20/g; s/&/%26/g'; }

# --- Relay: spool → batch report (ONSITE.md "The one wire contract", relay tier) ---------------
# The CGI receiver (hub-lite-cgi.sh) appends webhook lines to a spool; these functions roll the spool
# up into ONE batch POST. Wire contract: brvg-cloud-server/src/agentBatch.ts (v1); the canonical
# fixture there is what hub-lite/test.sh checks this output against.
#
# ⚠️ On the saving: an earlier version of this comment claimed the TLS handshake dominates, so
# collapsing connections saved most of the data. MEASURED IN PRODUCTION 2026-08-14 and that is
# wrong — a mains sensor reuses TLS sessions and costs ~551 B per report, not the ~5 KB a fresh
# handshake would. The roll-up saves tens of MB/month across a few sensors: worth having, not an
# order of magnitude. The relay's real justification is LOCKDOWN — sensors that hand off on the LAN
# need no WAN egress, so the forward chain can be deny-all with no allow rules.

# Spool lines (epoch	device	event	rawquery) → the JSON items array, deduped per device+event
# KEEPING THE NEWEST (a later line overwrites — the spool is append-ordered). stdout: one line,
# a JSON array. Decode + escape here must match hub-lite-cgi.sh byte for byte (tested).
spool_to_items() {
  awk -F'	' '
    function urldec(s,  out, i, c, h, hex) {
      hex = "0123456789abcdef"; gsub(/\+/, " ", s); out = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (c == "%" && i + 2 <= length(s)) {
          h = tolower(substr(s, i + 1, 2))
          if (h ~ /^[0-9a-f][0-9a-f]$/) {
            out = out sprintf("%c", (index(hex, substr(h, 1, 1)) - 1) * 16 + index(hex, substr(h, 2, 1)) - 1)
            i += 2; continue
          }
        }
        out = out c
      }
      return out
    }
    # gsub replacement escaping is its own trap: "\\\\" collapses to one backslash (a no-op).
    # "\\\\&" = a literal backslash, then the match — the portable way to double it.
    function jesc(s) { gsub(/\\/, "\\\\&", s); gsub(/"/, "\\\"", s); gsub(/[\001-\037]/, "", s); return s }
    function params_json(q,  n, parts, i, eq, k, v, out, first) {
      n = split(q, parts, "&"); out = "{"; first = 1
      for (i = 1; i <= n; i++) {
        eq = index(parts[i], "="); if (eq == 0) continue
        k = substr(parts[i], 1, eq - 1); v = urldec(substr(parts[i], eq + 1))
        gsub(/[^A-Za-z0-9_.-]/, "", k); if (k == "") continue
        if (!first) out = out ","; first = 0
        out = out "\"" k "\":\"" jesc(v) "\""
      }
      return out "}"
    }
    NF >= 3 {
      dev = $2; ev = $3; q = (NF >= 4 ? $4 : "")
      gsub(/[^A-Za-z0-9_.:-]/, "", dev); gsub(/[^A-Za-z0-9_.-]/, "", ev)
      if (dev == "" || ev == "") next
      key = dev SUBSEP ev
      if (!(key in seen)) { order[++count] = key; seen[key] = 1 }
      line[key] = "{\"device\":\"" dev "\",\"event\":\"" ev "\",\"params\":" params_json(q) "}"
    }
    END {
      printf "["
      for (i = 1; i <= count; i++) { if (i > 1) printf ","; printf "%s", line[order[i]] }
      printf "]"
    }'
}

# The devices present in a spool, one per line (for the ok/changed split).
spool_devices() {
  awk -F'	' 'NF >= 3 { gsub(/[^A-Za-z0-9_.:-]/, "", $2); if ($2 != "" && !(($2) in s)) { s[$2] = 1; print $2 } }'
}

# Assemble the envelope. $1=seq $2=kind $3=items-json-array $4=ok ids (space-separated) $5=boot id.
build_batch_json() {
  _ok="["
  _first=1
  for _d in $4; do
    [ "$_first" = 1 ] || _ok="$_ok,"
    _first=0
    _ok="$_ok\"$_d\""
  done
  _ok="$_ok]"
  printf '{"v":1,"seq":%s,"boot":"%s","kind":"%s","items":%s,"ok":%s,"agent":{"av":"%s","tier":"hub-lite"}}'     "$1" "$5" "$2" "${3:-[]}" "$_ok" "$HUB_LITE_VERSION"
}

RELAY_SPOOL="${BRVG_RELAY_SPOOL:-/tmp/brvg-relay.spool}"
RELAY_SEQ_FILE="${BRVG_RELAY_SEQ:-/tmp/brvg-relay.seq}"
RELAY_STATE_DIR="${BRVG_RELAY_STATE:-/tmp/brvg-relay-state}"
RELAY_BOOT_FILE="${BRVG_RELAY_BOOT:-/tmp/brvg-relay.boot}"

# Per-boot id, so the cloud can tell "this router rebooted and its counter restarted" from "this is
# a replay". WITHOUT IT THE COUNTER RESET IS SILENT DATA LOSS: /tmp is tmpfs on OpenWrt, so a power
# cut — routine on a boat — wipes the spool, the counter and the state together; the counter goes
# back to 1, the cloud still holds the old high-water mark, and every batch comes back
# `200 {duplicate:true}`. The drain below reads that as success and deletes the spool. See the
# `isNewSeq` comment in brvg-cloud-server/src/agentBatch.ts.
#
# The file lives in the SAME tmpfs as the counter, which is exactly what makes this correct: the id
# and the counter can only ever disappear together, so a new id always accompanies a reset.
relay_boot_id() {
  if [ -s "$RELAY_BOOT_FILE" ]; then cat "$RELAY_BOOT_FILE"; return 0; fi
  # The kernel's own per-boot UUID where it exists (every Linux since 2.6, OpenWrt included);
  # urandom, then pid+uptime, only so this can never return empty and emit `"boot":""`.
  # Readability tested first: `< missing 2>/dev/null` silences the COMMAND, not the shell's own
  # redirection error, so without the guard this prints to stderr on any host without /proc.
  _b=$([ -r /proc/sys/kernel/random/boot_id ] && tr -d '-' < /proc/sys/kernel/random/boot_id | cut -c1-32)
  [ -n "$_b" ] || _b=$(od -An -N8 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')
  [ -n "$_b" ] || _b="p$$u$(cut -d. -f1 /proc/uptime 2>/dev/null)"
  printf '%s' "$_b" | tr -cd 'A-Za-z0-9' > "$RELAY_BOOT_FILE"
  cat "$RELAY_BOOT_FILE"
}

# One drain: move the spool aside (an append that races lands in the NEXT drain), split devices
# into changed (items) vs unchanged (ok) against the last-sent state, POST, and only on success
# advance the sequence and the state. On failure the batch is prepended back for retry UNDER THE
# SAME SEQ — that is what lets the server drop a replay whole instead of re-alerting.
drain_relay() {
  # BOTH sources matter: fresh spool lines, AND a .sending file a failed drain left behind. Found
  # on the bench 2026-08-13: checking only the live spool meant a failed batch was never retried
  # until NEW telemetry arrived — on a quiet vessel, never.
  _sending="$RELAY_SPOOL.sending"
  [ -s "$RELAY_SPOOL" ] || [ -s "$_sending" ] || return 0
  # A previous failed drain left a .sending file — retry it first, oldest data wins.
  if [ ! -s "$_sending" ]; then
    mv "$RELAY_SPOOL" "$_sending" 2>/dev/null || return 0
  fi
  mkdir -p "$RELAY_STATE_DIR"
  _seq=$( (cat "$RELAY_SEQ_FILE" 2>/dev/null || echo 0) | tr -cd '0-9' )
  _seq=$(( ${_seq:-0} + 1 ))

  # Split: a device whose newest spooled line matches its last-SENT line has nothing new — it goes
  # in `ok` (freshness only). Everything else ships as items. Keyframe every Nth drain resends all.
  _kind="delta"
  [ $(( _seq % ${KEYFRAME_EVERY:-6} )) -eq 0 ] && _kind="keyframe"
  _items_src="$_sending.items"
  : > "$_items_src"
  _ok_ids=""
  while IFS= read -r _dev; do
    _newest=$(awk -F'	' -v d="$_dev" '$2 == d' "$_sending" | tail -1)
    _sig=$(printf '%s' "$_newest" | cut -f3-)
    _state="$RELAY_STATE_DIR/$_dev.last"
    if [ "$_kind" = "delta" ] && [ -f "$_state" ] && [ "$(cat "$_state")" = "$_sig" ]; then
      _ok_ids="${_ok_ids:+$_ok_ids }$_dev"
    else
      awk -F'	' -v d="$_dev" '$2 == d' "$_sending" >> "$_items_src"
    fi
  done <<EOF_DEVS
$(spool_devices < "$_sending")
EOF_DEVS

  _items=$(spool_to_items < "$_items_src")
  _body=$(build_batch_json "$_seq" "$_kind" "$_items" "$_ok_ids" "$(relay_boot_id)")
  _url="${WORKER_URL}/api/agent/batch?vid=${VID}&device=${DEVICE_ID}&t=${DEVICE_TOKEN}"
  # Same command piggyback + ack as send_event: the batch reply carries pending verbs, and the
  # request that delivers acks is the next one out — whichever path (event or batch) goes first.
  [ -n "$PENDING_ACK" ] && _url="${_url}&ack=${PENDING_ACK}"
  if _resp=$(curl -fsS --max-time 20 -X POST -H 'Content-Type: application/json' -d "$_body" "$_url" 2>/dev/null); then
    PENDING_ACK=""
    echo "$_seq" > "$RELAY_SEQ_FILE"
    # Persist last-sent per device so the next delta knows what "unchanged" means.
    while IFS= read -r _dev; do
      awk -F'	' -v d="$_dev" '$2 == d' "$_sending" | tail -1 | cut -f3- > "$RELAY_STATE_DIR/$_dev.last"
    done <<EOF_DEVS2
$(spool_devices < "$_sending")
EOF_DEVS2
    rm -f "$_sending" "$_items_src"
    log "relay: drained batch seq=$_seq ($_kind)"
    _cmds=$(printf '%s' "$_resp" | parse_commands)
    [ -n "$_cmds" ] && run_commands "$_cmds"
    # Config-as-state rides the same reply (cloud-server #100) — apply after commands so a
    # profile edit and a verb in one reply behave like the TS hub: verb runs, state lands.
    printf '%s' "$_resp" | lt_parse_profiles | lt_apply_profiles
  else
    rm -f "$_items_src"
    log "relay: batch seq=$_seq failed (will retry with the same seq)"
  fi
}

# --- Config ------------------------------------------------------------------------------------

CONF="${BRVG_HUB_LITE_CONF:-/etc/brvg-hub-lite.conf}"

load_config() {
  # shellcheck disable=SC1090
  [ -f "$CONF" ] && . "$CONF"
  WORKER_URL="${WORKER_URL:-https://api.dockneighbor.com}"
  GPS_INTERVAL="${GPS_INTERVAL:-120}"
  MODEM_INTERVAL="${MODEM_INTERVAL:-600}"
  [ "$GPS_INTERVAL" -lt 30 ] && GPS_INTERVAL=30       # floors: a metered link is not a firehose
  [ "$MODEM_INTERVAL" -lt 60 ] && MODEM_INTERVAL=60
  # Report-by-exception for GPS (scanning redesign, Phase 3). The fix is COLLECTED and the anchor
  # drag check RUN every GPS_INTERVAL regardless; these only gate the cloud SEND, to spare a metered
  # link when a boat is parked and unarmed. A fix is still sent whenever it moves past the deadband,
  # whenever an anchor watch is armed (always — the safety case), or once the liveness floor elapses
  # (so the cloud's offline detection and away-watch never go stale).
  GPS_DEADBAND_M="${GPS_DEADBAND_M:-50}"              # metres of movement before an unarmed send; 0 disables RBE
  GPS_LIVENESS_SECS="${GPS_LIVENESS_SECS:-1200}"      # a send at least this often (20 min < the 60-min offline default)
  [ "$GPS_DEADBAND_M" -lt 0 ] && GPS_DEADBAND_M=0
  [ "$GPS_LIVENESS_SECS" -lt 60 ] && GPS_LIVENESS_SECS=60
  AT_PORT="${AT_PORT:-/dev/ttyUSB2}"                  # GL-X750; X3000-class PCIe modems differ — see README
  GPS_SOURCE="${GPS_SOURCE:-auto}"                    # auto | at | gpsd | nmea
  GPS_DEVICE="${GPS_DEVICE:-}"                        # serial NMEA dongle for GPS_SOURCE=nmea
  if [ -z "$VID" ] || [ -z "$DEVICE_ID" ]; then
    echo "brvg-hub-lite: VID and DEVICE_ID are required in $CONF" >&2
    exit 1
  fi
  if [ -z "${DEVICE_TOKEN:-}" ] && [ -z "${VEHICLE_KEY:-}" ]; then
    echo "brvg-hub-lite: DEVICE_TOKEN (preferred) or VEHICLE_KEY is required in $CONF" >&2
    exit 1
  fi
  case "$WORKER_URL" in
    https://*) : ;;
    *) echo "brvg-hub-lite: WORKER_URL must be https" >&2; exit 1 ;;
  esac
}

log() { echo "brvg-hub-lite: $*" >&2; }

# --- AT transport (GL.iNet path — root on-device, straight to the modem port) ------------------

AT_BUF="${TMPDIR:-/tmp}/brvg-hub-lite.at.$$"

at_cmd() {
  # $1 = command, $2 = read window seconds (send_at blocks modem-side; GNSS reads answer fast)
  [ -c "$AT_PORT" ] || return 1
  : > "$AT_BUF"
  cat "$AT_PORT" > "$AT_BUF" 2>/dev/null &
  _cat=$!
  printf '%s\r' "$1" > "$AT_PORT" 2>/dev/null || { kill "$_cat" 2>/dev/null; return 1; }
  sleep "${2:-3}"
  kill "$_cat" 2>/dev/null
  wait "$_cat" 2>/dev/null
  cat "$AT_BUF"
}

# --- Collectors --------------------------------------------------------------------------------

detect_platform() {
  if [ -n "$PLATFORM" ]; then echo "$PLATFORM"
  elif [ -f /etc/glversion ] || [ -d /etc/gl-metadata ]; then echo glinet
  else echo generic
  fi
}

# A plugged-in USB GPS receiver, if any. This is the answer for routers whose modem has no GPS
# antenna port (hardware-verified on a GL-X750, 2026-08-06): a ~$15 u-blox dongle appears as a
# serial device streaming NMEA, and costs nothing to check for. Requires the kernel modules
# (kmod-usb-acm / kmod-usb-serial-*) — see hub-lite/README.md.
find_nmea_device() {
  [ -n "$GPS_DEVICE" ] && [ -c "$GPS_DEVICE" ] && { echo "$GPS_DEVICE"; return 0; }
  for _d in /dev/ttyACM0 /dev/ttyACM1 /dev/ttyUSB3 /dev/ttyUSB4; do
    [ "$_d" = "$AT_PORT" ] && continue          # never the modem's own AT port
    [ -c "$_d" ] || continue
    echo "$_d"; return 0
  done
  return 1
}

read_nmea_device() {
  _dev=$(find_nmea_device) || return 1
  # head -c bounds the read on a device that streams forever; timeout guards a silent one.
  timeout 6 head -c 4096 "$_dev" 2>/dev/null | parse_nmea_rmc
}

# NMEA over TCP — a chartplotter, AIS, gpsd, or a router serving NMEA on the LAN (GPS parity with
# the hub's NMEA_HOST source; owner sprint 2026-08-17). The hub-lite is always the CLIENT.
# ⚠️ BENCH-VERIFY before shipping to customers: busybox `nc` on FACTORY-STOCK GL.iNet firmware.
# The bench box has extra packages installed, so it proves nothing about a stock router — the same
# trap that made hand-installed Lua look like a working dependency.
read_gps_tcp() {
  [ -n "$GPS_HOST" ] || return 1
  command -v nc >/dev/null 2>&1 || { log "GPS_SOURCE=tcp needs nc (not found)"; return 1; }
  nc -w 8 "$GPS_HOST" "${GPS_PORT:-10110}" 2>/dev/null | head -n 40 | parse_nmea_rmc
}

# Cradlepoint NCOS local HTTP poll (the hub's CRADLEPOINT_HOST source, in shell): the router is
# POLLED, never configured to send anywhere (owner ruling 2026-08-17).
read_gps_cradlepoint() {
  [ -n "$CRADLEPOINT_HOST" ] || return 1
  curl -fsS --max-time 10 -u "${CRADLEPOINT_USER:-admin}:${CRADLEPOINT_PASSWORD:-}" \
    "http://${CRADLEPOINT_HOST}:${CRADLEPOINT_PORT:-80}/api/status/gps" 2>/dev/null | parse_cradlepoint_gps
}

collect_gps() {
  case "$GPS_SOURCE" in
    at) at_cmd 'AT+QGPSLOC=2' 3 | parse_qgpsloc ;;
    gpsd) command -v gpspipe >/dev/null 2>&1 && gpspipe -w -n 8 2>/dev/null | parse_gpsd_tpv ;;
    nmea) read_nmea_device ;;
    tcp) read_gps_tcp ;;
    cradlepoint) read_gps_cradlepoint ;;
    auto)
      # Modem GNSS first (no extra hardware), then a USB dongle, then gpsd. The fallback ORDER is
      # the point: a router with no GPS antenna port answers the AT read forever with "no fix",
      # so the dongle has to be tried even when the modem is present and healthy.
      _fix=""
      if [ "$(detect_platform)" = "glinet" ]; then
        _fix=$(at_cmd 'AT+QGPSLOC=2' 3 | parse_qgpsloc)
      fi
      if [ -z "$_fix" ]; then _fix=$(read_nmea_device); fi
      if [ -z "$_fix" ] && command -v gpspipe >/dev/null 2>&1; then
        _fix=$(gpspipe -w -n 8 2>/dev/null | parse_gpsd_tpv)
      fi
      [ -n "$_fix" ] && echo "$_fix" ;;
  esac
}

collect_modem() {
  # Only meaningful where an AT port exists (router / cellular HAT). "" elsewhere is fine —
  # a Pi hub on shore Wi-Fi simply has no modem story to tell.
  [ -c "$AT_PORT" ] || return 0
  sig=$(at_cmd 'AT+QCSQ' 2 | parse_qcsq)
  carrier=$(at_cmd 'AT+COPS?' 2 | parse_cops)
  sim=$(at_cmd 'AT+CPIN?' 2 | parse_cpin)
  data=$(at_cmd 'AT+QGDCNT?' 2 | parse_qgdcnt)
  echo "${sig}|${carrier}|${sim}|${data}"
}

# --- Anchor watch (local detection; the cloud stands down while we report) ---------------------
# The anchor alarm's LOGIC runs here, aboard, on every GPS tick — the cloud sweep only acts for
# boats with nothing local running (it sees our `anchorwatch=1` tag on gps reports and yields).
# Config arrives as `"anchor":{...}` on the report reply whenever the signature we report
# (`anchorsig`) differs from the vehicle's armed config — config-as-state, so a reboot self-heals:
# /tmp state is gone, the next report says sig 0, the reply re-arms us.
#
# Detection mirrors the app's reducer, not the cloud sweep's: we SEE A STREAM, so an alarm takes
# TWO consecutive fixes outside the radius by more than each fix's own reported accuracy. One
# borderline fix inside the GPS error bar never fires anything.

ANCHOR_STATE="${BRVG_ANCHOR_STATE:-/tmp/brvg-anchor.state}"       # "sig lat lon radiusM warnM"
ANCHOR_ALERTED="${BRVG_ANCHOR_ALERTED:-/tmp/brvg-anchor.alerted}" # sig whose ALARM already fired
ANCHOR_WARNED="${BRVG_ANCHOR_WARNED:-/tmp/brvg-anchor.warned}"    # sig whose WARNING already fired
ANCHOR_STREAK="${BRVG_ANCHOR_STREAK:-/tmp/brvg-anchor.streak}"    # consecutive alarm-breach fixes
ANCHOR_WSTREAK="${BRVG_ANCHOR_WSTREAK:-/tmp/brvg-anchor.wstreak}" # consecutive warn-breach fixes

# The signature of the watch we are running; "0" when disarmed. Reported on every gps tick.
anchor_sig() {
  set -- $(cat "$ANCHOR_STATE" 2>/dev/null)
  printf '%s' "${1:-0}"
}

# Pure: great-circle distance in whole meters (haversine; busybox awk has the trig).
anchor_distance() {
  awk -v la1="$1" -v lo1="$2" -v la2="$3" -v lo2="$4" 'BEGIN {
    r = 0.017453292519943295; R = 6371000;
    dla = (la2 - la1) * r; dlo = (lo2 - lo1) * r;
    sa = sin(dla / 2); sb = sin(dlo / 2);
    a = sa * sa + cos(la1 * r) * cos(la2 * r) * sb * sb;
    if (a > 1) a = 1;
    printf "%d", 2 * R * atan2(sqrt(a), sqrt(1 - a));
  }'
}

# Pure: pull the `"anchor":{...}` object off a report reply → "sig lat lon radiusM warnM" (a bare
# "0" for the stand-down, which the worker sends as {"sig":0}). Empty when the reply has none —
# the common case. Same tiny-sed approach as parse_commands: fixed shape, no JSON parser aboard.
parse_anchor() {
  _in=$(tr -d ' \n' | sed -n 's/.*"anchor":{\([^}]*\)}.*/\1/p')
  [ -z "$_in" ] && return 0
  _sig=$(printf '%s' "$_in" | sed -n 's/.*"sig":\(-\{0,1\}[0-9][0-9]*\).*/\1/p')
  [ -z "$_sig" ] && return 0
  if [ "$_sig" = "0" ]; then printf '0'; return 0; fi
  _la=$(printf '%s' "$_in" | sed -n 's/.*"lat":\(-\{0,1\}[0-9.][0-9.]*\).*/\1/p')
  _lo=$(printf '%s' "$_in" | sed -n 's/.*"lon":\(-\{0,1\}[0-9.][0-9.]*\).*/\1/p')
  _ra=$(printf '%s' "$_in" | sed -n 's/.*"radiusM":\([0-9][0-9]*\).*/\1/p')
  _wa=$(printf '%s' "$_in" | sed -n 's/.*"warnM":\([0-9][0-9]*\).*/\1/p')
  [ -z "$_la" ] || [ -z "$_lo" ] || [ -z "$_ra" ] && return 0
  printf '%s %s %s %s %s' "$_sig" "$_la" "$_lo" "$_ra" "${_wa:-0}"
}

# Adopt a config from the reply. A changed signature is a NEW EPISODE by construction: latches and
# streaks reset, exactly like the cloud sweep's re-arm semantics.
apply_anchor() {
  _new_sig="${1:-}"
  [ -z "$_new_sig" ] && return 0
  _cur=$(anchor_sig)
  [ "$_new_sig" = "$_cur" ] && return 0
  rm -f "$ANCHOR_ALERTED" "$ANCHOR_WARNED" "$ANCHOR_STREAK" "$ANCHOR_WSTREAK" 2>/dev/null
  if [ "$_new_sig" = "0" ]; then
    rm -f "$ANCHOR_STATE" 2>/dev/null
    log "anchor watch: disarmed by cloud config"
  else
    printf '%s %s %s %s %s' "$_new_sig" "$2" "$3" "$4" "${5:-0}" > "$ANCHOR_STATE"
    log "anchor watch: armed (radius ${4}m, warn ${5:-0}m, sig $_new_sig)"
  fi
}

# One ring's two-consecutive-fixes rule. $1 streak-file $2 latch-file $3 sig $4 dist $5 limit
# $6 event $7 extra-params. Fires at most once per episode; recovery inside the ring clears both.
anchor_ring() {
  if [ "$4" -gt "$5" ]; then
    _n=$(( $(cat "$1" 2>/dev/null | tr -cd '0-9') + 1 ))
    echo "$_n" > "$1"
    if [ "$_n" -ge 2 ] && [ "$(cat "$2" 2>/dev/null)" != "$3" ]; then
      log "anchor watch: $6 at ${4}m (limit ${5}m)"
      send_event "$6" "$7"
      echo "$3" > "$2"
    fi
  else
    rm -f "$1" 2>/dev/null
    [ -s "$2" ] && { rm -f "$2" 2>/dev/null; log "anchor watch: back inside — episode over"; }
  fi
}

# Evaluate one fix against the armed watch. $1 lat $2 lon $3 acc (may be empty → 0).
check_anchor() {
  [ -s "$ANCHOR_STATE" ] || return 0
  set -- $1 $2 ${3:-0} $(cat "$ANCHOR_STATE")
  _flat=$1; _flon=$2; _facc=$3; _sig=$4; _alat=$5; _alon=$6; _rad=$7; _warn=${8:-0}
  _d=$(anchor_distance "$_alat" "$_alon" "$_flat" "$_flon")
  # Beyond-accuracy rule per ring: the fix must be outside by MORE than its own error bar.
  _acc_i=$(printf '%s' "$_facc" | cut -d. -f1); _acc_i=${_acc_i:-0}
  anchor_ring "$ANCHOR_STREAK" "$ANCHOR_ALERTED" "$_sig" "$_d" $(( _rad + _acc_i )) \
    "anchor.motion" "dist=$_d&limit=$_rad"
  # Warning ring: only while the ALARM ring holds — the drag alarm says everything the warning
  # would. Cleared latches let a future drift warn again after recovery.
  if [ "$_warn" -gt 0 ] && [ "$_d" -le $(( _rad + _acc_i )) ]; then
    anchor_ring "$ANCHOR_WSTREAK" "$ANCHOR_WARNED" "$_sig" "$_d" $(( _warn + _acc_i )) \
      "anchor.warn.motion" "dist=$_d&limit=$_warn"
  fi
}

# --- Push --------------------------------------------------------------------------------------

# Pure: report URL for an event + pre-encoded params (exercised by test.sh). Token path wins.
build_report_url() {
  if [ -n "${DEVICE_TOKEN:-}" ]; then
    printf '%s/api/agent?vid=%s&device=%s&event=%s&t=%s&%s' "$WORKER_URL" "$VID" "$DEVICE_ID" "$1" "$DEVICE_TOKEN" "$2"
  else
    printf '%s/api/shelly?vid=%s&device=%s&event=%s&k=%s&%s' "$WORKER_URL" "$VID" "$DEVICE_ID" "$1" "$VEHICLE_KEY" "$2"
  fi
}

# --- LAN management door: shared state -----------------------------------------------------------
# The app talks to a hub-lite over HTTP on the LAN (uhttpd CGI, hub-lite-mgmt.sh) and falls back to
# the cloud command queue when it is not aboard. Both doors must describe the SAME router, so the
# reporting path writes what it just said into a state file and the CGI serves that file verbatim.
# Re-collecting in the CGI was the alternative and is worse in three ways: it would contend with
# this loop for the AT port, it would spend modem time on every page view, and the two paths could
# then disagree about the same instant.

# $1 = event name ("modem.measurement"), $2 = the urlencoded param string that was reported.
# Emits `"key":"value"` pairs; every value is quoted because a shell cannot tell a number from a
# string here and the app's parser already coerces (parseCachedModem).
state_pairs() {
  printf '%s' "$2" | tr '&' '\n' | awk -F= '
    $1 != "" && $2 != "" {
      gsub(/%20/, " ", $2); gsub(/"/, "", $2); gsub(/\\/, "", $2)
      # `av` is already in the object header. Emitting it again produced a DUPLICATE JSON KEY in
      # the real bench capture — legal-ish, last-one-wins in most parsers, and exactly the kind of
      # sloppiness that bites when a stricter parser meets it.
      if ($1 == "av") next
      printf "%s\"%s\":\"%s\"", (n++ ? "," : ""), $1, $2
    }'
}

write_state() {
  # $1 = event name, $2 = param string. Written to a temp file and moved into place so a reader
  # never sees a half-written object.
  _sf="${HUB_LITE_STATE}.$$"
  {
    printf '{"v":1,"event":"%s","ts":%s,"av":"%s"' "$1" "$(date +%s)" "$HUB_LITE_VERSION"
    _pairs=$(state_pairs "$1" "$2")
    [ -n "$_pairs" ] && printf ',%s' "$_pairs"
    printf '}\n'
  } > "$_sf" 2>/dev/null && mv "$_sf" "$HUB_LITE_STATE" 2>/dev/null
  rm -f "$_sf" 2>/dev/null
}

# --- LAN management door: the key ----------------------------------------------------------------
# One secret per ROUTER, minted and held by the worker (brvg-cloud-server/src/hubLiteKey.ts). We
# fetch it with the device token we already have, so a box enrolled before this feature existed
# picks its key up on the next tick with nothing to re-install and no re-enrollment.
#
# ⚠️ A hub-lite deliberately does NOT get the vehicle's per-member key set the way a full hub does.
# That set is every member's LAN management access and belongs on a host that can resolve roles;
# this is a router in a locker. One key, one router, one privilege level.
fetch_mgmt_key() {
  [ -n "${MGMT_KEY:-}" ] && return 0
  [ -n "${DEVICE_TOKEN:-}" ] || return 1        # the legacy VEHICLE_KEY path cannot ask for one
  _k=$(curl -fsS --max-time 10 \
    "${WORKER_URL}/api/agent/mgmt-key?vid=${VID}&device=${DEVICE_ID}&t=${DEVICE_TOKEN}" 2>/dev/null \
    | sed -n 's/.*"key":"\([0-9a-f]*\)".*/\1/p')
  case "$_k" in
    ????????????????????????????????????????????????????????????????) : ;;   # exactly 64 hex
    *) return 1 ;;
  esac
  MGMT_KEY="$_k"
  if [ -f "$CONF" ]; then
    _tmp="${CONF}.$$"
    grep -vE '^[[:space:]]*MGMT_KEY=' "$CONF" > "$_tmp" 2>/dev/null || true
    printf 'MGMT_KEY=%s\n' "$MGMT_KEY" >> "$_tmp"
    chmod 600 "$_tmp" 2>/dev/null
    mv "$_tmp" "$CONF"
  fi
  log "management key stored — the app can now reach this hub-lite directly on the LAN"
  return 0
}

# Commands acknowledged on the NEXT report (Phase B — see the worker's agentCommands.ts).
PENDING_ACK=""

# Set by run_commands when a verb's EFFECT needs to reach the cloud now rather than at the next
# tick. Commands arrive as the reply to a report, so the report that delivered them was composed
# BEFORE they ran — without this, "reset the data counter" shows the old counter for up to
# MODEM_INTERVAL, and "report now" does nothing observable at all. The main loop consumes it (see
# the note there): doing the follow-up send from inside send_event would re-enter send_event from
# its own body, and one failed curl mid-recursion loses the ack list.
FOLLOWUP_REPORT=0

# Pure: extract "id:cmd" pairs from the worker's JSON reply. Deliberately tiny — busybox has no
# JSON parser, and the payload shape is fixed and small: {"commands":[{"id":"..","cmd":".."}]}.
parse_commands() {
  tr -d ' \n' | sed -n 's/.*"commands":\[\(.*\)\].*/\1/p' \
    | sed 's/},{/}\n{/g' \
    | sed -n 's/.*"id":"\([A-Za-z0-9_-]*\)".*"cmd":"\([a-z_]*\)".*/\1:\2/p'
}

send_event() {
  # $1 = event name, $2 = pre-encoded param string ("lat=..&lon=..")
  _url=$(build_report_url "$1" "$2")
  [ -n "$PENDING_ACK" ] && _url="${_url}&ack=${PENDING_ACK}"
  _resp=$(curl -fsS --max-time 15 "$_url" 2>/dev/null)
  _rc=$?
  if [ $_rc -ne 0 ]; then
    # rc in the log (2026-08-17 bench): "failed" alone made a live send failure undiagnosable —
    # 22=HTTP error (worker rejected it: look at the URL), 6=DNS, 7/28=connectivity, 3=bad URL.
    log "send $1 failed (curl rc=$_rc; will retry next tick)"
    return 1
  fi
  PENDING_ACK=""   # the worker saw our acks; anything still queued comes back below
  _cmds=$(printf '%s' "$_resp" | parse_commands)
  [ -n "$_cmds" ] && run_commands "$_cmds"
  _anch=$(printf '%s' "$_resp" | parse_anchor)
  [ -n "$_anch" ] && apply_anchor $_anch
  return 0
}

# Apply a Cloud Update Schedule: $1 = gps seconds, $2 = modem seconds. Writes the config so a
# restart keeps it, and updates the running loop so it takes effect on the next tick rather than at
# the next reboot. The floors in load_config still apply — a metered link is not a firehose.
set_intervals() {
  log "command: interval -> gps ${1}s modem ${2}s"
  GPS_INTERVAL="$1"
  MODEM_INTERVAL="$2"
  [ "$GPS_INTERVAL" -lt 30 ] && GPS_INTERVAL=30
  [ "$MODEM_INTERVAL" -lt 60 ] && MODEM_INTERVAL=60
  if [ -f /etc/brvg-hub-lite.conf ]; then
    _tmp="/etc/brvg-hub-lite.conf.$$"
    grep -vE '^[[:space:]]*(GPS_INTERVAL|MODEM_INTERVAL)=' /etc/brvg-hub-lite.conf > "$_tmp" 2>/dev/null || true
    printf 'GPS_INTERVAL=%s\nMODEM_INTERVAL=%s\n' "$GPS_INTERVAL" "$MODEM_INTERVAL" >> "$_tmp"
    chmod 600 "$_tmp" 2>/dev/null
    mv "$_tmp" /etc/brvg-hub-lite.conf
  fi
  FOLLOWUP_REPORT=1
}

# Execute the allowlisted verbs the cloud queued. An unknown verb is acknowledged and DROPPED —
# never passed to a shell — so a queue this hub-lite doesn't understand can't become code execution.
run_commands() {
  for _entry in $1; do
    _id=${_entry%%:*}
    _cmd=${_entry#*:}
    case "$_cmd" in
      # FOLLOWUP_REPORT=1 on the verbs whose result is worth seeing immediately AND that leave the
      # uplink intact. Deliberately NOT set for reboot/reboot_modem (the link is about to drop, so
      # the extra send would just fail) or the update verbs (the hub-lite is being replaced).
      report_now)   log "command: report_now"; FOLLOWUP_REPORT=1 ;;
      self_update)  log "command: self_update"; self_update ;;
      rollback_agent) log "command: rollback_agent"; restore_hub_lite "requested" ;;
      reboot)       log "command: reboot"; (sleep 5; reboot) >/dev/null 2>&1 & ;;
      reboot_modem) log "command: reboot_modem"; at_cmd 'AT+CFUN=1,1' 5 >/dev/null 2>&1 ;;
      reset_data)   log "command: reset_data"; at_cmd 'AT+QGDCNT=0' 3 >/dev/null 2>&1; FOLLOWUP_REPORT=1 ;;
      # HIGH SECURITY: local administration off. A router bolted to a marina pole is physically
      # reachable by anyone; with SSH and the vendor web UI down, a thief's only route in is a
      # factory reset, which yields a blank router rather than this vehicle's network.
      #
      # This is SAFE BY CONSTRUCTION: a command can only reach us as the reply to a report that
      # SUCCEEDED, so cloud reachability is proven at the moment we disable the local doors. There
      # is no path where an already-offline router locks itself out.
      #
      # Recovery is deliberate, not accidental: factory reset, then restore from the app.
      local_admin_off)
        log "command: local_admin_off — disabling ssh + vendor web UI"
        uci set dropbear.@dropbear[0].enable='0' 2>/dev/null && uci commit dropbear 2>/dev/null
        /etc/init.d/dropbear stop 2>/dev/null
        /etc/init.d/dropbear disable 2>/dev/null
        # The vendor UI is nginx on GL.iNet 4.x; uhttpd on stock OpenWrt. Stop whichever exists.
        for _svc in nginx uhttpd; do
          [ -x "/etc/init.d/$_svc" ] || continue
          "/etc/init.d/$_svc" stop 2>/dev/null
          "/etc/init.d/$_svc" disable 2>/dev/null
        done
        FOLLOWUP_REPORT=1 ;;
      local_admin_on)
        log "command: local_admin_on — restoring ssh + vendor web UI"
        uci set dropbear.@dropbear[0].enable='1' 2>/dev/null && uci commit dropbear 2>/dev/null
        /etc/init.d/dropbear enable 2>/dev/null
        /etc/init.d/dropbear start 2>/dev/null
        for _svc in nginx uhttpd; do
          [ -x "/etc/init.d/$_svc" ] || continue
          "/etc/init.d/$_svc" enable 2>/dev/null
          "/etc/init.d/$_svc" start 2>/dev/null
        done
        FOLLOWUP_REPORT=1 ;;
      # Cloud Update Schedule pushed from the app. ONE VERB PER SCHEDULE, not a verb with a value:
      # parse_commands matches [a-z_]+ only, and the whole allowlist property is that a command can
      # never carry an argument. Persisted to the config so it survives a restart, and applied to the
      # live loop without one. GPS keeps its 30 s floor — the drag-detection rule needs it.
      interval_saver)    set_intervals 1800 3600 ;;
      interval_regular)  set_intervals 900 1800 ;;
      interval_often)    set_intervals 300 600 ;;
      interval_constant) set_intervals 30 300 ;;
      # Traffic lockdown on/off (Phase B first verbs, owner sprint 2026-08-17). ON re-arms the
      # watchdog's released marker so a later hub death can release again; OFF is the same
      # release the watchdog performs. Both safe-by-construction: they arrive only as the reply
      # to a report that SUCCEEDED, so cloud reachability is proven at the moment they run.
      lockdown_on)
        log "command: lockdown_on"
        if apply_lockdown; then rm -f "$HUB_WATCH_RELEASED" 2>/dev/null; else log "lockdown_on: uci unavailable"; fi
        FOLLOWUP_REPORT=1 ;;
      lockdown_off)
        log "command: lockdown_off"
        release_lockdown || log "lockdown_off: nothing to release"
        FOLLOWUP_REPORT=1 ;;
      gps_on)       log "command: gps_on"
                    at_cmd 'AT+QGPSCFG="autogps",1' 3 >/dev/null 2>&1
                    at_cmd 'AT+QGPS=1' 3 >/dev/null 2>&1
                    FOLLOWUP_REPORT=1 ;;
      *)            log "command: ignoring unknown verb" ;;
    esac
    PENDING_ACK="${PENDING_ACK:+$PENDING_ACK,}$_id"
  done
}

# --- Self-update ------------------------------------------------------------------------------
# Deliberately argument-free. The command channel carries a verb and nothing else, so there is no
# attacker-controlled string anywhere in this path: no URL, no version, no filename.

hub_lite_path() {
  # Where this script is installed. Falls back to the packaged path.
  command -v brvg-hub-lite 2>/dev/null || echo /usr/bin/brvg-hub-lite
}

# Restore the kept-back copy of the previous hub-lite. Used both by the rollback verb and
# automatically when a freshly installed hub-lite fails its smoke check.
restore_hub_lite() {
  _self=$(hub_lite_path)
  if [ ! -s "$HUB_LITE_BACKUP" ]; then
    log "rollback: no previous version kept ($1)"
    return 1
  fi
  cp "$HUB_LITE_BACKUP" "$_self" && chmod 0755 "$_self" || { log "rollback: copy failed"; return 1; }
  log "rolled back to the previous hub-lite ($1); restarting"
  [ -x /etc/init.d/brvg-hub-lite ] && (sleep 2; /etc/init.d/brvg-hub-lite restart) >/dev/null 2>&1 &
  return 0
}

self_update() {
  if ! command -v opkg >/dev/null 2>&1; then
    # The Pi hub installs from git/systemd, not opkg. Refuse rather than inventing a second,
    # unsigned update path on the platform that has no package signing.
    log "self_update: no opkg on this platform — skipping"
    return 0
  fi
  _self=$(hub_lite_path)
  # Keep the running hub-lite so a bad release is one command (or one failed smoke check) from undone.
  cp "$_self" "$HUB_LITE_BACKUP" 2>/dev/null && chmod 0644 "$HUB_LITE_BACKUP" 2>/dev/null
  # What we are running BEFORE opkg touches anything — the only reliable way to tell an actual
  # upgrade from a no-op. See the restart guard below.
  _before=$("$_self" --version 2>/dev/null)

  # opkg verifies the feed's usign signature itself; --no-check-certificate and friends are
  # deliberately NOT used. A feed that fails its signature check simply does not install.
  if ! opkg update >/dev/null 2>&1; then
    log "self_update: feed refresh failed (offline, or the feed signature did not verify)"
    return 1
  fi
  if ! opkg upgrade brvg-hub-lite >/dev/null 2>&1; then
    log "self_update: no upgrade applied (already current, or the package failed verification)"
    return 1
  fi

  # Smoke-check the thing we just installed BEFORE trusting it to keep the vehicle reporting.
  if ! "$_self" --version >/dev/null 2>&1; then
    log "self_update: the new hub-lite failed its version check"
    restore_hub_lite "failed smoke check"
    return 1
  fi
  # 🔴 ONLY RESTART IF THE VERSION ACTUALLY MOVED.
  #
  # `opkg upgrade <pkg>` EXITS 0 WHEN THE PACKAGE IS ALREADY CURRENT — it has nothing to do and
  # says so successfully. So the exit status above cannot distinguish "upgraded" from "nothing to
  # do", and this function used to log "installed <same version>; restarting" and bounce the
  # collector anyway. Measured on sc4-lab 2026-09-03, on a router already at 0.14.7.
  #
  # That is not cosmetic. The operator console has a per-hub FORCE UPDATE button (and a fleet
  # rollout is meant to sweep every device), so pressing it on a fleet that is already current
  # would restart every router's collector for no reason — a small silent gap on each one, caused
  # by an update that did nothing.
  _after=$("$_self" --version 2>/dev/null)
  if [ "$_before" = "$_after" ]; then
    log "self_update: already current ($_after) — nothing installed, not restarting"
    return 0
  fi
  log "self_update: installed $_after (was ${_before:-unknown}); restarting"
  [ -x /etc/init.d/brvg-hub-lite ] && (sleep 2; /etc/init.d/brvg-hub-lite restart) >/dev/null 2>&1 &
  return 0
}

# --- Hub watchdog: fail open rather than leave the vessel silent ------------------------------
# Owner decision 2026-08-13. Under lockdown deny-all the hub is the ONLY path to the cloud. When
# the hub runs ON this router the failure domains coincide (a dead router means a dead gateway
# anyway) — but when it runs on a Pi/Docker/desktop it can die while the router routes happily,
# and the vessel goes silent with no remote way back in.
#
# So the ROUTER watches the hub: it is the enforcement point, and it is independently alive. After
# HUB_WATCH_FAILS consecutive failed health checks it RELEASES lockdown (availability beats
# lockdown purity) and reports it.
#
# The event names are chosen, not invented. `hub.offline` / `hub.online` match the worker's
# existing /offline|online/ → health rule INTENTIONALLY, so this needs no notifyCategories change.
# Checked against the real rules table, and the near-miss is instructive: `lockdown.released` also
# classifies — but only because "lockdown" happens to contain the substring "down" from the
# /…|down|fall/ rule. That is an accident, and it would evaporate the day someone tightens that
# rule to \bdown\b, silently sending a "your firewall was released" alert to NOBODY
# (categoryForEvent returns null → delivered to no one). Match on purpose, not by coincidence.
#
# It deliberately does NOT re-arm on recovery: a hub that restarts every few minutes would flap the
# firewall (each apply is a ~10 s reload). The released state is announced; re-arming is one tap in
# the app.

HUB_WATCH_FAILS_FILE="${BRVG_HUB_FAILS:-/tmp/brvg-hub-watch.fails}"
HUB_WATCH_RELEASED="${BRVG_HUB_RELEASED:-/tmp/brvg-hub-watch.released}"

# PURE: given the consecutive-failure count, the threshold, whether we already released, and
# whether the last probe succeeded — what should happen? Echoes: release | recover | none
watch_decide() {
  _healthy="$1"; _fails="$2"; _threshold="$3"; _released="$4"
  if [ "$_healthy" = "1" ]; then
    [ "$_released" = "1" ] && { echo recover; return 0; }
    echo none; return 0
  fi
  if [ "$_released" = "1" ]; then echo none; return 0; fi
  if [ "$_fails" -ge "$_threshold" ]; then echo release; return 0; fi
  echo none
}

# Remove every lockdown rule. The `brvg_lk_` NAME PREFIX is the shared contract between this and
# the app's SSH enforcement (src-tauri/src/lockdown.rs) — matching on the prefix, not on shared
# code, is what lets two languages manage the same rules without drifting.
release_lockdown() {
  command -v uci >/dev/null 2>&1 || return 1
  _i=0; _del=""
  while uci -q get "firewall.@rule[$_i]" >/dev/null 2>&1; do
    case "$(uci -q get "firewall.@rule[$_i].name")" in brvg_lk_*) _del="$_i $_del";; esac
    _i=$((_i + 1))
  done
  [ -n "$_del" ] || return 1          # nothing of ours applied — nothing to release
  for _j in $_del; do uci delete "firewall.@rule[$_j]" 2>/dev/null; done
  uci commit firewall
  /etc/init.d/firewall reload >/dev/null 2>&1 || true
  return 0
}

# Argument-free traffic lockdown: ONE catch-all REJECT rule (lan->wan), named under the shared
# brvg_lk_ prefix so the app's SSH enforcement, the watchdog, and release_lockdown all manage the
# same set. fw3/fw4 consult rules before zone forwardings, so this closes the forward chain while
# the hub (OUTPUT, not FORWARD) keeps reporting. Per-MAC allow rules stay app-applied — a verb
# carries no arguments, so it can only express the no-allows shape.
apply_lockdown() {
  command -v uci >/dev/null 2>&1 || return 1
  release_lockdown >/dev/null 2>&1 || true   # idempotent re-apply; hand-written rules survive
  _n=$(uci add firewall rule) || return 1
  uci set "firewall.$_n.name=brvg_lk_deny_all"
  uci set "firewall.$_n.src=lan"
  uci set "firewall.$_n.dest=wan"
  uci set "firewall.$_n.proto=any"
  uci set "firewall.$_n.target=REJECT"
  uci commit firewall
  /etc/init.d/firewall reload >/dev/null 2>&1 || true
  return 0
}

# --- Lockdown: the LAN door's richer half --------------------------------------------------------
# `apply_lockdown` above is the CLOUD verb: argument-free by design, so it can only ever express
# the no-allows shape. The LAN door is authenticated by the management key and can carry the
# per-MAC allow list, which is what the app needed SSH for until now (src-tauri/src/lockdown.rs).
#
# ⚠️ THIS REPLACES A REMOTE SHELL WITH A TYPED CALL, so the validation below is the whole point:
# the app used to generate a uci script and pipe it into `ssh root@router`. Here a MAC that is not
# a MAC is rejected before it reaches a uci argument, and there is no path by which a caller's
# string becomes a command.

# PURE: is this a hardware address? Rejects everything else, including the empty string.
valid_mac() {
  case "$1" in
    [0-9A-Fa-f][0-9A-Fa-f]:[0-9A-Fa-f][0-9A-Fa-f]:[0-9A-Fa-f][0-9A-Fa-f]:[0-9A-Fa-f][0-9A-Fa-f]:[0-9A-Fa-f][0-9A-Fa-f]:[0-9A-Fa-f][0-9A-Fa-f]) return 0 ;;
    *) return 1 ;;
  esac
}

# Most rules one apply may write. Mirrors MAX_ALLOW_MACS in the app's Rust: a vessel has ~a dozen
# devices, and far past that is a bug rather than a boat.
LOCKDOWN_MAX_MACS=64

# What the router is ACTUALLY enforcing, in the exact text the app's tested parser expects
# (parseLockdownState in dashboard/src/utils/lockdownTransport.ts). Emitting the raw `uci show`
# lines rather than a summary of our own is deliberate: it keeps ONE parser for both doors, so the
# SSH path and this one can never disagree about what is applied.
lockdown_show() {
  command -v uci >/dev/null 2>&1 || { echo BRVG_LK_NONE; return 0; }
  uci show firewall 2>/dev/null | grep -E 'brvg_lk|src_mac' || echo BRVG_LK_NONE
}

# $1 = 1 to write the catch-all, 0 to remove everything of ours. $2.. = allow MACs.
# Rule ORDER is the mechanism: fw3/fw4 consult `config rule` entries before zone forwardings and in
# creation order, so every ACCEPT is written before the REJECT that follows it.
lockdown_apply_rules() {
  command -v uci >/dev/null 2>&1 || return 1
  _catch="$1"; shift
  [ $# -le "$LOCKDOWN_MAX_MACS" ] || { log "lockdown: too many approved devices"; return 2; }
  for _m in "$@"; do
    # Never echo the offending value — a count is enough for a log, and the value came off the wire.
    valid_mac "$_m" || { log "lockdown: an approved-device entry is not a hardware address"; return 2; }
  done

  release_lockdown >/dev/null 2>&1 || true   # a full rewrite of OUR rules; hand-written ones survive
  if [ "$_catch" = "1" ]; then
    _guest=0
    uci show firewall 2>/dev/null | grep -q "name='guest'" && _guest=1
    _i=0
    for _m in "$@"; do
      _n=$(uci add firewall rule) || return 1
      uci set "firewall.$_n.name=brvg_lk_allow_$_i"
      uci set "firewall.$_n.src=lan"
      uci set "firewall.$_n.dest=wan"
      uci set "firewall.$_n.src_mac=$_m"
      uci set "firewall.$_n.target=ACCEPT"
      uci set "firewall.$_n.proto=all"
      if [ "$_guest" = "1" ]; then
        _n=$(uci add firewall rule) || return 1
        uci set "firewall.$_n.name=brvg_lk_allow_g$_i"
        uci set "firewall.$_n.src=guest"
        uci set "firewall.$_n.dest=wan"
        uci set "firewall.$_n.src_mac=$_m"
        uci set "firewall.$_n.target=ACCEPT"
        uci set "firewall.$_n.proto=all"
      fi
      _i=$((_i + 1))
    done
    # The catch-alls go LAST — uci preserves creation order, and the rules run in it.
    _n=$(uci add firewall rule) || return 1
    uci set "firewall.$_n.name=brvg_lk_deny"
    uci set "firewall.$_n.src=lan"
    uci set "firewall.$_n.dest=wan"
    uci set "firewall.$_n.target=REJECT"
    uci set "firewall.$_n.proto=all"
    if [ "$_guest" = "1" ]; then
      _n=$(uci add firewall rule) || return 1
      uci set "firewall.$_n.name=brvg_lk_deny_guest"
      uci set "firewall.$_n.src=guest"
      uci set "firewall.$_n.dest=wan"
      uci set "firewall.$_n.target=REJECT"
      uci set "firewall.$_n.proto=all"
    fi
  fi
  uci commit firewall
  /etc/init.d/firewall reload >/dev/null 2>&1 || true
  # Bench 2026-08-13: rules take effect ~10 s AFTER reload returns. Report state only once what we
  # report is what the router is doing.
  sleep 11
  return 0
}

watch_hub() {
  # BANDWIDTH SAVER MODE FAILS CLOSED (owner, 2026-08-17): when lockdown exists to control
  # metered-SIM spend, a dead hub must NOT release it — silence until the connectivity-offline
  # alert is the accepted failure mode. The gate also skips the probe itself: no curl per tick.
  [ "${BANDWIDTH_SAVER:-0}" = "1" ] && return 0
  [ -n "${HUB_WATCH_URL:-}" ] || return 0
  _threshold="${HUB_WATCH_FAILS:-5}"
  _fails=$( (cat "$HUB_WATCH_FAILS_FILE" 2>/dev/null || echo 0) | tr -cd '0-9' )
  _fails=${_fails:-0}
  _released=0; [ -f "$HUB_WATCH_RELEASED" ] && _released=1

  if curl -fsS --max-time 8 "$HUB_WATCH_URL" >/dev/null 2>&1; then
    _healthy=1; _fails=0
  else
    _healthy=0; _fails=$((_fails + 1))
  fi
  echo "$_fails" > "$HUB_WATCH_FAILS_FILE"

  case "$(watch_decide "$_healthy" "$_fails" "$_threshold" "$_released")" in
    release)
      if release_lockdown; then
        : > "$HUB_WATCH_RELEASED"
        log "hub unreachable ${_fails}x — RELEASED lockdown so the vehicle keeps reporting"
        send_event "hub.offline" "released=1" || true
      else
        log "hub unreachable ${_fails}x — no lockdown rules to release"
        : > "$HUB_WATCH_RELEASED"   # don't retry the release every tick
        send_event "hub.offline" "released=0" || true
      fi
      ;;
    recover)
      rm -f "$HUB_WATCH_RELEASED"
      log "hub is answering again (network restrictions stay OFF until re-applied in the app)"
      send_event "hub.online" "rearmed=0" || true
      ;;
  esac
}

# Pure-ish: should this fix be SENT to the cloud? (Reads the last-sent globals below.) Report-by-
# exception, Phase 3: skip a send only when the boat is unarmed, hasn't moved past the deadband, and
# the liveness floor hasn't elapsed. NEVER gates the local drag check — the caller runs that anyway.
#   $1 lat  $2 lon  $3 anchor sig ("0" = unarmed)
gps_should_send() {
  [ "$3" != "0" ] && return 0                         # armed ⇒ always send (the safety case)
  [ -z "$GPS_LAST_LAT" ] && return 0                  # first fix of this run ⇒ establish the baseline
  _now=$(date +%s)
  [ $(( _now - ${GPS_LAST_SENT:-0} )) -ge "$GPS_LIVENESS_SECS" ] && return 0   # liveness floor
  [ "$GPS_DEADBAND_M" -le 0 ] && return 0             # RBE disabled ⇒ send every tick
  _moved=$(anchor_distance "$GPS_LAST_LAT" "$GPS_LAST_LON" "$1" "$2")
  [ "${_moved:-0}" -ge "$GPS_DEADBAND_M" ] && return 0
  return 1
}

# $1 = "force" to bypass the deadband (a command follow-up / report_now wants a fresh line out).
push_gps() {
  _force="${1:-}"
  set -- $(collect_gps)
  [ -z "$1" ] && { log "no GPS fix this tick"; return 0; }
  _glat=$1; _glon=$2; _gacc=${3:-}
  # anchorsig on EVERY report (the worker replies with the config when we're stale — including
  # the stand-down); anchorwatch=1 only while armed, which is what tells the cloud sweep a local
  # watcher owns the anchor logic and it should yield.
  _asig=$(anchor_sig)
  if [ "$_force" = "force" ] || gps_should_send "$_glat" "$_glon" "$_asig"; then
    _p="lat=$_glat&lon=$_glon"
    [ -n "$_gacc" ] && _p="$_p&acc=$_gacc"
    _p="$_p&anchorsig=$_asig"
    [ "$_asig" != "0" ] && _p="$_p&anchorwatch=1"
    send_event "gps.measurement" "$_p"
    GPS_LAST_LAT=$_glat; GPS_LAST_LON=$_glon; GPS_LAST_SENT=$(date +%s)
  fi
  # ALWAYS, whether or not we sent: local drag detection must never depend on the network. A config
  # adopted from a send's reply (when there was one) evaluates against this same fix.
  check_anchor "$_glat" "$_glon" "$_gacc"
}

# --- WAN usage accounting -----------------------------------------------------------------------
# The modem's own counter (AT+QGDCNT) tells us CELLULAR bytes, which is the right basis for plan
# alerts. It cannot tell us anything about the other WANs — so a week on marina Wi-Fi reads as
# "the counter didn't move", and we have no way to show the uplink or the roll-up actually saved
# anything. These per-interface counters close that.
#
# THE HARD PART IS RESETS, not reading. /sys counters are since-boot, so:
#   * a reboot sends them to 0 — a naive delta would be hugely negative;
#   * an interface bounce (modem reconnect, repeater rejoin) resets that one alone;
#   * our own `reset_data` verb deliberately zeroes the modem counter.
# So every delta is computed against a stored previous value, and a value that went DOWN is
# treated as a reset: report the NEW value as the delta (bytes since the reset) and carry on,
# rather than emitting a negative or a wrap-around-sized spike.

WAN_STATE_DIR="${BRVG_WAN_STATE:-/tmp/brvg-wan-state}"

# PURE: previous, current -> bytes to report. Echoes the delta.
#
# ⚠️ FIRST SIGHT REPORTS ZERO, and that is deliberate. It used to report `$_cur` — "everything so
# far" — which is harmless after a REBOOT (the kernel's /sys counters reset with the state dir, so
# `$_cur` is near zero) but badly wrong on a FRESH INSTALL: the state dir doesn't exist yet while
# the interface counters hold the router's ENTIRE UPTIME, potentially weeks of traffic. That whole
# figure landed in the billing cycle as a single tick — under the worker's 50 GB sanity cap, so
# nothing rejected it — and could trip the 80%/100% plan alerts, pushing a false "you have used
# your data plan" the moment a customer onboarded a router that had been running a while.
#
# Those bytes moved before we were watching, and probably before the cycle began, so they are not
# ours to attribute. Record the baseline, report nothing, start counting from the next tick.
wan_delta() {
  _prev="$1"; _cur="$2"
  [ -n "$_prev" ] || { echo 0; return 0; }                # first sight: baseline only, report none
  if [ "$_cur" -lt "$_prev" ] 2>/dev/null; then
    echo "$_cur"                                          # counter reset — count from zero
  else
    echo $(( _cur - _prev ))
  fi
}

# Which interface is carrying the default route right now: cellular | wired | wifi | none.
# This is what lets the cloud attribute bytes to a SOURCE rather than just an interface name.
wan_kind() {
  case "$1" in
    wwan*|wwan0|rmnet*|usb*) echo cellular ;;
    eth0|wan|eth0.2)         echo wired ;;
    apcli*|sta*|wlan*)       echo wifi ;;
    *)                       echo other ;;
  esac
}

# Read one interface's total bytes (rx+tx), or nothing if it has no counters.
wan_bytes() {
  _rx="/sys/class/net/$1/statistics/rx_bytes"
  _tx="/sys/class/net/$1/statistics/tx_bytes"
  [ -r "$_rx" ] && [ -r "$_tx" ] || return 1
  echo $(( $(cat "$_rx") + $(cat "$_tx") ))
}

# The interface currently holding the default route (empty if offline).
wan_active_iface() {
  ip route show default 2>/dev/null | awk '/^default/ { for (i=1;i<=NF;i++) if ($i=="dev") { print $(i+1); exit } }'
}

# LAN-side guard: an interface enslaved to a bridge carries LOCAL client traffic, not WAN bytes.
# Bench GL-X750 2026-08-17: wlan0/wlan1 are the router's own APs (Mode: Master) and sit in br-lan
# alongside the eth1 LAN port — all three expose sysfs `brport`; the true WAN faces (wwan0, eth0)
# do not. A repeater/STA uplink is the wan interface of its firewall zone, never a br-lan member,
# so it still counts. $1 = the /sys/class/net/<if> path.
wan_lan_side() { [ -e "$1/brport" ]; }

# Emit "&wanKb_cellular=<kb>&wanKb_wifi=<kb>..." for the SOURCES that moved since last tick, plus
# the active source. One param per kind — two radios both classifying as wifi used to emit
# `wanKb_wifi` twice (seen live 2026-08-17), and the wire contract has no duplicate keys. Only
# NON-ZERO totals are sent — a boat on Wi-Fi shouldn't pay for a cellular field that says 0.
collect_wan_usage() {
  mkdir -p "$WAN_STATE_DIR" 2>/dev/null
  _out=""
  _active=$(wan_active_iface)
  [ -n "$_active" ] && _out="&wanSrc=$(wan_kind "$_active")"
  _kb_cellular=0; _kb_wired=0; _kb_wifi=0
  for _path in /sys/class/net/*; do
    [ -e "$_path" ] || continue                           # no match ⇒ the glob stayed literal
    _if=$(basename "$_path")
    case "$_if" in lo|br-*) continue ;; esac              # loopback, the LAN bridge itself
    wan_lan_side "$_path" && continue                     # bridge ports: AP radios + LAN ports
    _kind=$(wan_kind "$_if")
    [ "$_kind" = "other" ] && continue                    # only WAN-side media
    _cur=$(wan_bytes "$_if") || continue
    _f="$WAN_STATE_DIR/$_if"
    _prev=$(cat "$_f" 2>/dev/null | tr -cd '0-9')
    _d=$(wan_delta "$_prev" "$_cur")
    echo "$_cur" > "$_f"
    # Report in KB: MB loses a slow trickle entirely, raw bytes waste URL length every tick.
    _kb=$(( _d / 1024 ))
    case "$_kind" in
      cellular) _kb_cellular=$(( _kb_cellular + _kb )) ;;
      wired)    _kb_wired=$(( _kb_wired + _kb )) ;;
      wifi)     _kb_wifi=$(( _kb_wifi + _kb )) ;;
    esac
  done
  [ "$_kb_cellular" -gt 0 ] && _out="$_out&wanKb_cellular=${_kb_cellular}"
  [ "$_kb_wired" -gt 0 ] && _out="$_out&wanKb_wired=${_kb_wired}"
  [ "$_kb_wifi" -gt 0 ] && _out="$_out&wanKb_wifi=${_kb_wifi}"
  printf '%s' "$_out"
}

push_modem() {
  _m=$(collect_modem)
  [ -z "$_m" ] && return 0
  _sig=${_m%%|*}; _rest=${_m#*|}
  _carrier=${_rest%%|*}; _rest=${_rest#*|}
  _sim=${_rest%%|*}; _data=${_rest#*|}
  set -- $_sig
  _p="up=1"
  [ -n "$1" ] && _p="$_p&mode=$(urlencode_spaces "$1")"
  [ -n "$2" ] && _p="$_p&rssi=$2"
  [ -n "$3" ] && _p="$_p&rsrp=$3"
  [ -n "$4" ] && _p="$_p&sinr=$4"
  [ -n "$5" ] && _p="$_p&rsrq=$5"
  [ -n "$_carrier" ] && _p="$_p&carrier=$(urlencode_spaces "$_carrier")"
  [ -n "$_sim" ] && _p="$_p&sim=$_sim"
  # Plan-burn: the modem counts bytes since its last reset; the cloud turns that into the
  # 80%/100%-of-plan alerts, so report MB rather than raw bytes.
  if [ -n "$_data" ]; then
    set -- $_data
    if [ -n "$1" ] && [ -n "$2" ]; then
      _mb=$(( ($1 + $2) / 1048576 ))
      _p="$_p&dataMb=$_mb"
    fi
  fi
  # Report which hub-lite version is running, plus per-source WAN usage. Staged rollout and rollback
  # are unmanageable without the version: you cannot decide who to update next if you cannot see
  # what is deployed.
  _p="$_p&av=$HUB_LITE_VERSION$(collect_wan_usage)"
  # State BEFORE the send: what this router knows about itself is true whether or not the WAN is
  # up, and the LAN door is exactly the door that still works when the cloud send fails.
  write_state "modem.measurement" "$_p"
  send_event "modem.measurement" "$_p"
}

# --- LinkTap: local flood -> valve shutoff (hub-lite capability #1; owner 2026-08-19) -----------
# The hub-only LinkTap model (ONSITE.md "LinkTap — hub-only, over local HTTP", 2026-08-19): the gateway lives on the LAN and this
# router is its controller. When a flood alarm arrives at the relay's receiver, close every
# configured valve over the gateway's local HTTP API BEFORE the cloud send — the close must not
# wait on the WAN, and with the LinkTap cloud gone this is the only automated close path when the
# uplink is down. The valve self-limits regardless (every open carries duration+volume), so this
# only ever closes it sooner. Same capability as the TypeScript hub's floodStopAll — a second
# implementation by design; the shared fixtures in test.sh keep the two from diverging.

# The worker's events.ts flood-shutoff line, ported verbatim: /flood|leak|alarm/i, minus clears
# (_off / .off), minus telemetry (.measurement / .change). Keep the three in the same order so a
# diff against events.ts stays readable.
is_flood_shutoff() {
  _ev=$(printf '%s' "$1" | tr 'A-Z' 'a-z')
  case "$_ev" in
    *.measurement|*.change) return 1 ;;
    *_off|*.off) return 1 ;;
  esac
  case "$_ev" in
    *flood*|*leak*|*alarm*) return 0 ;;
  esac
  return 1
}

# The cmd 7 body, same dialect as the TS hub's buildStop — {"cmd":7,"gw_id":...,"dev_id":...}.
linktap_stop_body() {
  printf '{"cmd":7,"gw_id":"%s","dev_id":"%s"}' "$1" "$2"
}

# Close every valve in $LINKTAP_DEV_IDS via http://$LINKTAP_HOST/api.shtml. No-op when LinkTap is
# not configured, so every existing install is untouched. Each attempt spools a
# linktap.flood_close.change line (rides the roll-up — visibility with zero new wire surface) and
# logs locally; a failed close is spooled with ok=0 rather than retried here — the alarm itself is
# already on its way to the cloud, and the worker's own flood path remains the escalation.
linktap_flood_close() {
  [ -n "${LINKTAP_HOST:-}" ] && [ -n "${LINKTAP_GW_ID:-}" ] && [ -n "${LINKTAP_DEV_IDS:-}" ] || return 0
  for _d in $(printf '%s' "$LINKTAP_DEV_IDS" | tr ',' ' '); do
    # Canonical 16-hex id, same normalisation as the TS client's normalizeDevId.
    _d=$(printf '%s' "$_d" | tr -cd 'A-Za-z0-9' | cut -c1-16)
    [ -n "$_d" ] || continue
    if curl -fsS --max-time 5 -X POST -H 'Content-Type: application/json'         -d "$(linktap_stop_body "$LINKTAP_GW_ID" "$_d")"         "http://${LINKTAP_HOST}/api.shtml" >/dev/null 2>&1; then
      _ok=1
    else
      _ok=0
    fi
    printf '%s\t%s\t%s\t%s\n' "$(date +%s)" "lt_${_d}" "linktap.flood_close.change" "ok=${_ok}" \
      >> "${BRVG_RELAY_SPOOL:-/tmp/brvg-relay.spool}"
    logger -t brvg-hub-lite "flood shutoff: valve ${_d} close ok=${_ok}" 2>/dev/null || true
  done
}

# --- LinkTap: cycle semantics on hub-lite (parity port of hub/src/cycle.ts) ---------------------
# Owner doctrine 2026-08-19: "hub lite should do anything a hub can do as long as it is not
# CPU/memory restrictive." This is the schedules port: the SAME decision table as the TypeScript
# cycle machine — the shared fixtures in test.sh mirror test/cycle.test.ts case for case, which is
# what keeps two implementations from diverging (the one-contract rule).
#
# Scope of this increment: NORMAL RUNS only (poll, software volume cutoff, end-reason
# classification, restart-only-on-timer, adoption of external opens). Washdown/tank fill stay
# app/hub-driven; the ledger stays on the TS hub. State lives in tmpfs — a reboot loses it and the
# ADOPTION rule rebuilds it from the gateway's own answer, exactly like the TS hub's restart rule.

# Parse a cmd 3 reply (possibly HTML-wrapped) to "watering volumeL remain". $1 = vol unit
# (gal|L). Volume is converted to LITRES here so every comparison downstream is one unit; the
# idle garbage latch (>100000 — a closed GW-02 sat at 15.9M) reads as 0, never as water.
lt_parse_status() {
  awk -v unit="$1" '
    { buf = buf $0 }
    END {
      w = 0
      if (buf ~ /"is_watering":[[:space:]]*(true|1|"true"|"1")/) w = 1
      vol = 0
      if (match(buf, /"volume":[[:space:]]*[0-9.]+/)) {
        v = substr(buf, RSTART, RLENGTH); sub(/.*:/, "", v); vol = v + 0
        if (vol < 0 || vol > 100000) vol = 0
        else if (unit == "gal") vol = vol * 3.785411784
      }
      rem = ""
      if (match(buf, /"remain_duration":[[:space:]]*[0-9.]+/)) {
        r = substr(buf, RSTART, RLENGTH); sub(/.*:/, "", r); rem = int(r + 0)
      }
      # Instantaneous flow, same unit as volume (gateway unit per MINUTE) -> L/min. Feeds the
      # cutoff lead time; 0 for missing/absurd, which simply disables the lead.
      spd = 0
      if (match(buf, /"speed":[[:space:]]*[0-9.]+/)) {
        sp = substr(buf, RSTART, RLENGTH); sub(/.*:/, "", sp); spd = sp + 0
        if (spd < 0 || spd > 100000) spd = 0
        else if (unit == "gal") spd = spd * 3.785411784
      }
      printf "%d %.3f %s %.3f\n", w, vol, rem, spd
    }'
}

# The decision table, PURE — mirrors cycle.ts step() + shouldAutoRestart(). Args:
#   $1 prev ("idle" | "watering"), $2 now-watering (0/1), $3 volumeL, $4 capL (0 = none),
#   $5 stop_issued ("" | volume_cap | manual | flood_shutoff), $6 elapsedSecs, $7 durationSecs,
#   $8 speedLpm (OPTIONAL, 0/absent = no lead)
# Prints ONE word: adopt | cut | none | ended:<reason>
#
# ⚠️ THE CUT FIRES EARLY, BY THE STOP LATENCY — mirrors daemon cycle.rs `cutoff_trigger_l`, and the
# two must not drift. The hardware ignores `volume_limit` (proven inert on GW-02 2026-08-22), so
# this cutoff is the only volume enforcement there is; firing it AT the cap overshoots by whatever
# still flows while the stop lands — measured 0.79 gal at 5.83 gal/min, i.e. ~8 s. Lead by
# `speed x 8s`, clamped at 0, and fall back to the cap exactly when no speed is known.
lt_decide() {
  _prev="$1"; _now="$2"; _vol="$3"; _cap="$4"; _stop="$5"; _elapsed="$6"; _dur="$7"; _speed="${8:-0}"
  if [ "$_prev" = "idle" ]; then
    [ "$_now" = "1" ] && { echo adopt; return; }
    echo none; return
  fi
  if [ "$_now" = "1" ]; then
    # The software cutoff, fired EARLY by the stop latency (see the note above the function).
    if [ -z "$_stop" ] && awk -v v="$_vol" -v c="$_cap" -v s="$_speed" \
        'BEGIN{ if (c <= 0) exit 1; t = c; if (s > 0) { t = c - s * (8.0/60.0); if (t < 0) t = 0 } exit !(v >= t) }'; then
      echo cut; return
    fi
    echo none; return
  fi
  # Closed. Classify: what we DID outranks inference (the order that fixes the restart bug).
  if [ -n "$_stop" ]; then echo "ended:$_stop"; return; fi
  if awk -v v="$_vol" -v c="$_cap" 'BEGIN{exit !(c > 0 && v >= c)}'; then echo "ended:volume_cap"; return; fi
  if [ "$_elapsed" -ge $(( _dur - 60 )) ] 2>/dev/null; then echo "ended:timer"; return; fi
  echo "ended:unknown"
}

# ONLY a timer expiry of a NORMAL run restarts. $1 reason, $2 enabled (0/1), $3 mode.
#
# ⚠️ THE MODE CHECK IS THE WHOLE POINT AND IT WAS MISSING. The daemon's rule is
# `auto_restart_enabled && ended.mode == Mode::Normal && ended.reason == EndReason::Timer`
# (cycle.rs should_auto_restart); this tested reason and the switch only. Latent while hub-lite ran
# Normal Runs exclusively — and a live water-safety bug the moment washdown exists here, because a
# washdown ending on its own timer would restart as ANOTHER washdown, uncapped, forever.
#
# Mode absent reads as normal: that is what every state file written before this change means, and
# treating an old file as a washdown would silently stop honouring auto-restart on upgrade.
lt_should_restart() {
  [ "$2" = "1" ] || return 1
  [ "$1" = "timer" ] || return 1
  case "${3:-normal}" in normal|'') return 0 ;; *) return 1 ;; esac
}

# cmd 6 body — duration SECONDS, volume_limit in the GATEWAY unit ($3 already converted).
lt_start_body() {
  printf '{"cmd":6,"gw_id":"%s","dev_id":"%s","duration":%d,"volume_limit":%s}' "$1" "$2" "$3" "$4"
}

# Per-valve profiles from the worker reply (config-as-state; the same {linktap:{profiles}} blob
# the TypeScript hub consumes — worker cloud-server #100). One line per valve:
#   <devid> <durationSecs|-> <volumeCapL|-> <autoRestart 0/1/->
# "-" = the vehicle never set that field: the conf default keeps it (skip-don't-default,
# preserved end to end). Deliberately tiny awk — busybox has no JSON parser; the inner objects
# are flat, so [^{}]* is exact, and the walk stops at the brace that closes "profiles" so a later
# object-valued key in the reply can never be misread as a valve.
lt_parse_profiles() {
  awk '
    { buf = buf $0 }
    END {
      if (!match(buf, /"linktap":[[:space:]]*\{[[:space:]]*"profiles":[[:space:]]*\{/)) exit
      rest = substr(buf, RSTART + RLENGTH)
      while (match(rest, /^[[:space:],]*"[A-Za-z0-9]+":[[:space:]]*\{[^{}]*\}/)) {
        e = substr(rest, RSTART, RLENGTH)
        rest = substr(rest, RSTART + RLENGTH)
        id = e; sub(/^[[:space:],]*"/, "", id); sub(/".*/, "", id); id = substr(id, 1, 16)
        dur = "-"; vol = "-"; ar = "-"
        if (match(e, /"durationSecs":[[:space:]]*[0-9.]+/)) { v = substr(e, RSTART, RLENGTH); sub(/.*:/, "", v); dur = int(v + 0) }
        if (match(e, /"volumeCapL":[[:space:]]*[0-9.]+/))   { v = substr(e, RSTART, RLENGTH); sub(/.*:/, "", v); vol = v + 0 }
        if (match(e, /"autoRestart":[[:space:]]*(true|false)/)) { v = substr(e, RSTART, RLENGTH); ar = (v ~ /true/) ? 1 : 0 }
        if (id != "") print id, dur, vol, ar
      }
    }'
}

# Persist parsed profiles into $LT_STATE_DIR/profile.<dev>. Whole-file rewrite per valve named in
# the reply: the worker recomputes the blob from the vehicle each delivery, so what arrives IS the
# truth for those valves; valves it does not name keep whatever they had (their conf default).
lt_apply_profiles() {
  mkdir -p "$LT_STATE_DIR" 2>/dev/null
  while read -r _pid _pdur _pvol _par; do
    [ -n "$_pid" ] || continue
    {
      [ "$_pdur" != "-" ] && echo "P_DUR=$_pdur"
      [ "$_pvol" != "-" ] && echo "P_VOL=$_pvol"
      [ "$_par"  != "-" ] && echo "P_AR=$_par"
    } > "$LT_STATE_DIR/profile.$_pid"
  done
}

# The DAILY LEDGER (parity port of cycle.ts applyToLedger — the last hub-lite gap, 2026-08-19).
# Owner rule: washdown volume does NOT count against the daily value; everything else does,
# including an adopted manual run — a hose run by hand is exactly the water the number exists to
# see. Day keys are UTC ISO dates (storage is UTC, display converts — house rule).
#
# State per valve in $LT_STATE_DIR/ledger.<dev>: "DAY=YYYY-MM-DD" + "DAY_VOL=<litres>". tmpfs, so
# a reboot loses it — acceptable and honest: the ledger is a running total the cloud also receives
# on every measurement, so the cloud's copy is the durable one.

# $1 mode, $2 volume litres, $3 day key, $4 ledger file. Prints the new running total.
lt_ledger_apply() {
  _lm="$1"; _lv="$2"; _lday="$3"; _lfile="$4"
  DAY=""; DAY_VOL=0
  # shellcheck disable=SC1090
  [ -f "$_lfile" ] && . "$_lfile"
  # A new UTC day starts from zero rather than carrying yesterday's total forward.
  [ "$DAY" = "$_lday" ] || DAY_VOL=0
  # Washdown contributes nothing (owner rule) — but it still ROLLS the day, so the file is never
  # left holding a stale date that would make tomorrow's first Normal Run resume yesterday's total.
  if [ "$_lm" = "washdown" ]; then
    _add=0
  else
    _add="$_lv"
  fi
  DAY_VOL=$(awk -v a="$DAY_VOL" -v b="$_add" 'BEGIN{printf "%.2f", a + b}')
  printf 'DAY=%s\nDAY_VOL=%s\n' "$_lday" "$DAY_VOL" > "$_lfile"
  printf '%s' "$DAY_VOL"
}

# UTC day key. `date -u +%F` is POSIX and present on busybox.
lt_day_key() { date -u +%F; }

# One poll pass over every configured valve. State per valve in $LT_STATE_DIR/<dev>:
#   state=idle|watering  started=<epoch>  stop= |volume_cap|manual|flood_shutoff
#   mode=normal|washdown|tankfill   dur=<secs>   cap=<litres>
#
# mode/dur/cap describe THIS RUN, not the profile: a washdown is time-only with a different length
# than the vehicle's Normal Run, so the profile cannot answer for it. All three are absent in files
# written before 2026-08-31 and default to the profile's Normal Run, which is what those files
# meant.
# Load one valve's persisted cycle into the machine's variables.
#
# 🔴 EXTRACTED BECAUSE THE BUG THAT LIVED HERE WAS UNTESTABLE INLINE. The state file writes `state=`
# and the machine reads `_state`; sourcing sets the UNPREFIXED name, so the persisted cycle never
# loaded — _state was `idle` on every tick. `lt_decide` only evaluates the software volume cutoff on
# the `_prev != idle` branch, so it returned `adopt` forever and THE CUTOFF NEVER FIRED, on the tier
# whose own comment calls it "the only volume enforcement there is".
#
# Sets: _state _started _stop _mode _dur_eff _cap_eff. $1 = state file, $2 = profile duration,
# $3 = profile cap. Everything is cleared first, because these are set by SOURCING and would
# otherwise leak from the previous valve in a multi-valve loop.
lt_load_state() {
  state=""; started=""; stop=""; mode=""; dur=""; cap=""
  # shellcheck disable=SC1090
  [ -f "$1" ] && . "$1"
  _state="${state:-idle}"
  _started="${started:-0}"
  _stop="${stop:-}"
  # This run's own targets win over the profile's; absent means "a Normal Run on the profile",
  # which is exactly what a state file written before 2026-09-01 meant.
  _mode="${mode:-normal}"
  _dur_eff="${dur:-$2}"
  _cap_eff="${cap:-$3}"
}

LT_STATE_DIR="${LT_STATE_DIR:-/tmp/brvg-linktap}"

linktap_tick() {
  [ -n "${LINKTAP_HOST:-}" ] && [ -n "${LINKTAP_GW_ID:-}" ] && [ -n "${LINKTAP_DEV_IDS:-}" ] || return 0
  mkdir -p "$LT_STATE_DIR" 2>/dev/null
  # Read the gateway's volume unit ONCE per boot (a config change needs a gateway visit anyway).
  if [ ! -f "$LT_STATE_DIR/unit" ]; then
    _u=$(curl -fsS --max-time 10 -X POST -H 'Content-Type: application/json' \
      -d "{\"cmd\":16,\"gw_id\":\"$LINKTAP_GW_ID\"}" "http://${LINKTAP_HOST}/api.shtml" 2>/dev/null \
      | grep -o '"vol_unit":"[^"]*"' | cut -d'"' -f4)
    # Default GALLONS when unreadable — guessing litres under-reports the cap 3.79x (TS readVolUnit).
    [ "$_u" = "L" ] || _u="gal"
    echo "$_u" > "$LT_STATE_DIR/unit"
  fi
  _unit=$(cat "$LT_STATE_DIR/unit")

  for _d in $(printf '%s' "$LINKTAP_DEV_IDS" | tr ',' ' '); do
    _d=$(printf '%s' "$_d" | tr -cd 'A-Za-z0-9' | cut -c1-16)
    [ -n "$_d" ] || continue
    # Effective profile: the wire profile's fields over the conf defaults, FIELD BY FIELD —
    # the same profileFor rule as the TS hub.
    _dur="${LINKTAP_NORMAL_SECS:-86400}"
    _capL="${LINKTAP_NORMAL_VOL_L:-378}"
    _ar="${LINKTAP_AUTO_RESTART:-0}"
    if [ -f "$LT_STATE_DIR/profile.$_d" ]; then
      P_DUR=""; P_VOL=""; P_AR=""
      # shellcheck disable=SC1090
      . "$LT_STATE_DIR/profile.$_d"
      [ -n "$P_DUR" ] && _dur="$P_DUR"
      [ -n "$P_VOL" ] && _capL="$P_VOL"
      [ -n "$P_AR" ]  && _ar="$P_AR"
    fi
    _reply=$(curl -fsS --max-time 10 -X POST -H 'Content-Type: application/json' \
      -d "{\"cmd\":3,\"gw_id\":\"$LINKTAP_GW_ID\",\"dev_id\":\"$_d\"}" \
      "http://${LINKTAP_HOST}/api.shtml" 2>/dev/null) || continue
    set -- $(printf '%s' "$_reply" | lt_parse_status "$_unit")
    _w="$1"; _volL="$2"; _speedL="${4:-0}"

    _sf="$LT_STATE_DIR/$_d"
    lt_load_state "$_sf" "$_dur" "$_capL"
    _elapsed=$(( $(date +%s) - _started ))

    _act=$(lt_decide "$_state" "$_w" "$_volL" "$_cap_eff" "$_stop" "$_elapsed" "$_dur_eff" "$_speedL")
    case "$_act" in
      adopt)
        # Manual press / external open IS a Normal Run with the profile cap (owner rule).
        # An ADOPTED cycle is a Normal Run on the profile by the owner's rule, so it records those
        # targets explicitly rather than leaving them absent and inheriting whatever comes next.
        printf 'state=watering\nstarted=%s\nstop=\nmode=normal\ndur=%s\ncap=%s\n' \
          "$(date +%s)" "$_dur" "$_capL" > "$_sf"
        logger -t brvg-hub-lite "linktap: adopted a running cycle on ${_d} (Normal Run cap ${_capL}L)" 2>/dev/null || true
        ;;
      cut)
        curl -fsS --max-time 5 -X POST -H 'Content-Type: application/json' \
          -d "$(linktap_stop_body "$LINKTAP_GW_ID" "$_d")" "http://${LINKTAP_HOST}/api.shtml" >/dev/null 2>&1
        # Keep the run's identity through the stop, so the close that follows classifies against
        # the cycle that was actually running rather than a default Normal Run.
        printf 'state=watering\nstarted=%s\nstop=volume_cap\nmode=%s\ndur=%s\ncap=%s\n' \
          "$_started" "$_mode" "$_dur_eff" "$_cap_eff" > "$_sf"
        logger -t brvg-hub-lite "linktap: volume cap ${_capL}L reached on ${_d} — stop issued" 2>/dev/null || true
        ;;
      ended:*)
        _reason="${_act#ended:}"
        rm -f "$_sf"
        # The cycle's MODE decides whether it counts, and washdown does NOT (daemon cycle.rs
        # apply_to_ledger, owner rule). The note that used to sit here said this tier only ran
        # Normal Runs and that adding washdown must not silently miscount — this is that change
        # honouring its own warning.
        _dayvol=$(lt_ledger_apply "$_mode" "$_volL" "$(lt_day_key)" "$LT_STATE_DIR/ledger.$_d")
        printf '%s\t%s\t%s\t%s\n' "$(date +%s)" "lt_${_d}" "linktap.cycle.change" \
          "reason=${_reason}&vol_l=${_volL}&day=$(lt_day_key)&day_vol_l=${_dayvol}" \
          >> "${BRVG_RELAY_SPOOL:-/tmp/brvg-relay.spool}"
        if lt_should_restart "$_reason" "$_ar" "$_mode"; then
          _capGw=$(awk -v c="$_capL" -v u="$_unit" 'BEGIN{printf "%.2f", (u=="gal") ? c/3.785411784 : c}')
          curl -fsS --max-time 5 -X POST -H 'Content-Type: application/json' \
            -d "$(lt_start_body "$LINKTAP_GW_ID" "$_d" "$_dur" "$_capGw")" "http://${LINKTAP_HOST}/api.shtml" >/dev/null 2>&1 \
            && printf 'state=watering\nstarted=%s\nstop=\nmode=normal\ndur=%s\ncap=%s\n' \
                 "$(date +%s)" "$_dur" "$_capL" > "$_sf"
          logger -t brvg-hub-lite "linktap: timer expired on ${_d}, auto-restart on — fresh Normal Run" 2>/dev/null || true
        fi
        ;;
      none) : ;;
    esac
    # Telemetry rides the roll-up, same event name as the TS hub.
    # Same event name and same params as the TS hub / daemon emit, so the cloud cannot tell the
    # tiers apart — which is the point of the one-contract rule.
    _ldg=""
    if [ -f "$LT_STATE_DIR/ledger.$_d" ]; then
      DAY=""; DAY_VOL=0
      # shellcheck disable=SC1090
      . "$LT_STATE_DIR/ledger.$_d"
      _ldg="&day=${DAY}&day_vol_l=${DAY_VOL}"
    fi
    printf '%s\t%s\t%s\t%s\n' "$(date +%s)" "lt_${_d}" "linktap.measurement" \
      "watering=${_w}&vol_l=${_volL}${_ldg}" \
      >> "${BRVG_RELAY_SPOOL:-/tmp/brvg-relay.spool}"
  done
}

# --- Main loop ---------------------------------------------------------------------------------

main() {
  load_config
  log "starting (platform=$(detect_platform), gps every ${GPS_INTERVAL}s, modem every ${MODEM_INTERVAL}s)"
  # Small random start offset so a fleet doesn't tick in lockstep after a regional power event.
  sleep $(( $$ % 20 ))
  # An urgent webhook (alarm) pokes the drain immediately — aggregation must never delay one that
  # the CGI failed to deliver directly.
  [ "${HUB_LITE_ENABLED:-0}" = "1" ] && trap drain_relay USR1
  _elapsed=$MODEM_INTERVAL   # first loop sends both
  _lt_elapsed=${LINKTAP_POLL:-120}   # first loop polls the gateway too
  while :; do
    push_gps
    if [ "$_lt_elapsed" -ge "${LINKTAP_POLL:-120}" ]; then
      linktap_tick
      _lt_elapsed=0
    fi
    _lt_elapsed=$(( _lt_elapsed + GPS_INTERVAL ))
    if [ "$_elapsed" -ge "$MODEM_INTERVAL" ]; then
      push_modem
      # No-op once we have a key. Here rather than at startup so a box that boots with no WAN still
      # collects one the moment the uplink comes back. NOT gated on HUB_LITE_ENABLED: that flag is
      # the RELAY TIER, and the management door is not part of it.
      fetch_mgmt_key
      [ "${HUB_LITE_ENABLED:-0}" = "1" ] && drain_relay
      watch_hub
      _elapsed=0
    fi
    # A verb the LAN door ran in its own process asked for a follow-up report (see
    # HUB_LITE_FOLLOWUP above). Consumed here, where FOLLOWUP_REPORT actually means something.
    if [ -f "$HUB_LITE_FOLLOWUP" ]; then
      rm -f "$HUB_LITE_FOLLOWUP"
      FOLLOWUP_REPORT=1
    fi
    # A command ran during the sends above. Its effect is NOT in the report that carried it — that
    # payload was built first — so report again before sleeping. Cleared before sending, so a
    # command arriving in the follow-up is handled by the next pass rather than looping here; at
    # most one extra pair of sends per iteration, whatever the queue does.
    if [ "$FOLLOWUP_REPORT" = "1" ]; then
      FOLLOWUP_REPORT=0
      log "command follow-up: reporting the new state"
      push_gps force   # a follow-up / report_now wants a fresh line, deadband notwithstanding
      push_modem
      _elapsed=0
    fi
    sleep "$GPS_INTERVAL"
    _elapsed=$(( _elapsed + GPS_INTERVAL ))
  done
}

# `--version` must work without a config: the self-update smoke check runs it on a freshly
# installed hub-lite before that hub-lite has ever been configured.
case "${1:-}" in
  --version|-v) echo "$HUB_LITE_VERSION"; exit 0 ;;
esac

# Sourced by hub-lite/test.sh with BRVG_HUB_LITE_TEST set — parsers only, no loop, no config.
if [ -z "$BRVG_HUB_LITE_TEST" ]; then
  main "$@"
fi
