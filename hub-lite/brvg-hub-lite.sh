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

HUB_LITE_VERSION="0.15.0"
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

# The same flag-file pattern, for a CONFIG change made by a CGI (/api/hub/config, /token, /clear,
# /bootstrap). The collector sources its conf once at start, so a CGI that rewrote it must ask the
# running loop to read it again, or the new heartbeat or GPS source would wait for a restart.
HUB_LITE_RELOAD="${BRVG_HUB_LITE_RELOAD:-/tmp/brvg-hub-lite.reload}"

# Epoch the service was last STARTED (written by init.d start_service, and by the collector when
# absent). Two readers: `uptimeSecs` on /api/hub/status, and the first-run window /api/hub/bootstrap
# honours. tmpfs, so a reboot or a service restart reopens the window, exactly like the daemon's.
HUB_LITE_STARTED="${BRVG_HUB_LITE_STARTED:-/tmp/brvg-hub-lite.started}"

# The newest version the signed feed offers, when it is newer than this one (update_check). Absent
# means current or unknown. Visibility only; installing is still the argument-free self_update.
HUB_LITE_UPDATE="${BRVG_HUB_LITE_UPDATE:-/tmp/brvg-hub-lite.update}"


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

# NMEA sentence(s) → "lat lon [acc]" from the freshest valid fix (ddmm.mmmm → decimal degrees).
#
# Mirrors the daemon's gps.rs parse_nmea, rule for rule: RMC is preferred (the LAST valid one), GGA
# is the fallback and carries HDOP x 5 m as a rough accuracy, and any sentence that FAILS its
# checksum is skipped. A sentence with no checksum at all is accepted, because forwarders strip it.
#
# ⚠️ THE CHECKSUM IS NOT PEDANTRY. This parser reads a TCP stream it joined mid-sentence and a USB
# port it cut off after 4 KB, so a torn line is the normal case, not the exotic one. A torn RMC can
# still carry `A` and a plausible-looking number, and on an armed anchor watch a position a
# kilometre off is a false drag alarm.
#
# XOR is done by hand: mawk (ubuntu's awk, which CI runs) and POSIX awk have no bitwise operators.
parse_nmea_rmc() {
  awk '
    BEGIN { for (i = 32; i < 127; i++) ord[sprintf("%c", i)] = i; HEX = "0123456789ABCDEF" }
    function xor8(a, b,  r, bit, i) {
      r = 0; bit = 1
      for (i = 0; i < 8; i++) { if ((a % 2) != (b % 2)) r += bit; a = int(a / 2); b = int(b / 2); bit *= 2 }
      return r
    }
    function sum_ok(line,  star, i, c, hx) {
      star = 0
      for (i = length(line); i > 1; i--) if (substr(line, i, 1) == "*") { star = i; break }
      if (star == 0) return 1
      c = 0
      for (i = 2; i < star; i++) c = xor8(c, ord[substr(line, i, 1)] + 0)
      hx = toupper(substr(line, star + 1, 2))
      if (hx !~ /^[0-9A-F][0-9A-F]$/) return 0
      return (index(HEX, substr(hx, 1, 1)) - 1) * 16 + index(HEX, substr(hx, 2, 1)) - 1 == c
    }
    function coord(raw, hemi,  v, d, m) {
      if (raw == "" || hemi == "") return ""
      v = raw + 0; d = int(v / 100); m = v - d * 100
      if (m >= 60) return ""
      v = d + m / 60
      if (hemi == "S" || hemi == "W") v = -v
      return v
    }
    {
      line = $0; gsub(/[\r\n]/, "", line); sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
      if (substr(line, 1, 1) != "$" || !sum_ok(line)) next
      body = line; sub(/\*.*$/, "", body)
      n = split(body, f, ",")
      tag = substr(f[1], length(f[1]) - 2)
      if (tag == "RMC" && n >= 7 && f[3] == "A") {
        la = coord(f[4], f[5]); lo = coord(f[6], f[7])
        if (la == "" || lo == "" || (la == 0 && lo == 0)) next
        # %.5f (≈1 m), matching the modem path: awk default OFMT is %.6g, which drops the USB dongle
        # to ~4 decimals (~11 m). Verified live on a u-blox 7 (bench 2026-08-13).
        rmc = sprintf("%.5f %.5f", la, lo)
      } else if (tag == "GGA" && n >= 9 && (f[7] + 0) > 0) {
        la = coord(f[3], f[4]); lo = coord(f[5], f[6])
        if (la == "" || lo == "" || (la == 0 && lo == 0)) next
        hd = f[9] + 0
        gga = (hd > 0) ? sprintf("%.5f %.5f %.0f", la, lo, hd * 5) : sprintf("%.5f %.5f", la, lo)
      }
    }
    END { if (rmc != "") print rmc; else if (gga != "") print gga }'
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

# The most lines the spool (and, separately, a failed batch waiting in .sending) may hold. The
# daemon's MAX_SHELLY_QUEUE, the same number for the same reason: sensors report on events and
# wake-ups, so a few hundred lines covers hours of a busy boat, and a bound is what stops a week-long
# outage filling a router's RAM (tmpfs) until the box falls over — the one outcome worse than
# losing old readings.
RELAY_SPOOL_MAX="${BRVG_RELAY_SPOOL_MAX:-300}"

# PURE: is this spooled event an ALARM, kept longest when the spool must shed? The daemon's
# shelly_is_alarm: the flood rule, plus anything the device itself calls an alarm, flood or leak.
# Everything else that is not telemetry (a button press, an alarm clear) counts as well, because the
# receiver sent it NOW rather than batching it, which is the same judgement made one step earlier.
spool_is_alarm() {
  _sa=$(printf '%s' "$1" | tr 'A-Z' 'a-z')
  case "$_sa" in
    *alarm*|*flood*|*leak*) return 0 ;;
    *.measurement|*.change) return 1 ;;
  esac
  return 0
}

# Bound a spool file to $2 lines (default RELAY_SPOOL_MAX), shedding the OLDEST READINGS first and
# an alarm only when nothing but alarms is left — the daemon's enqueue_shelly rule. Two passes over
# the same file in one awk: the first counts, the second decides. The rewrite goes through a temp
# file and a mv, so a receiver appending concurrently loses at most the line that raced the mv —
# and that line was a reading appended to a spool already over its bound.
spool_cap() {
  _sc_f="$1"; _sc_max="${2:-$RELAY_SPOOL_MAX}"
  [ -s "$_sc_f" ] || return 0
  _sc_n=$(wc -l < "$_sc_f" | tr -cd '0-9')
  [ "${_sc_n:-0}" -gt "$_sc_max" ] || return 0
  _sc_tmp="$_sc_f.cap.$$"
  awk -F'	' -v max="$_sc_max" '
    function alarm(ev,  e) {
      e = tolower(ev)
      if (e ~ /alarm|flood|leak/) return 1
      if (e ~ /\.(measurement|change)$/) return 0
      return 1
    }
    NR == FNR { n++; if (!alarm($3)) readings++; next }
    FNR == 1 { drop = n - max; if (drop < 0) drop = 0; shed_r = (drop < readings) ? drop : readings; shed_a = drop - shed_r }
    {
      if (!alarm($3) && shed_r > 0) { shed_r--; next }
      if (alarm($3) && shed_a > 0) { shed_a--; next }
      print
    }' "$_sc_f" "$_sc_f" > "$_sc_tmp" 2>/dev/null && mv "$_sc_tmp" "$_sc_f"
  rm -f "$_sc_tmp" 2>/dev/null
  log "relay: spool over ${_sc_max} lines - shed $(( _sc_n - _sc_max )) (oldest readings first)"
}

# Does the spool hold anything that must not wait for the next modem tick — a batch that already
# failed once, or an alarm the receiver could not deliver directly? The main loop drains on the GPS
# tick while this holds, which is the daemon's short shelly_retry_loop in shell-sized form.
relay_needs_retry() {
  [ -s "$RELAY_SPOOL.sending" ] && return 0
  [ -s "$RELAY_SPOOL" ] || return 1
  awk -F'	' '{ e = tolower($3); if (e ~ /alarm|flood|leak/ || e !~ /\.(measurement|change)$/) { f = 1; exit } } END { exit !f }' "$RELAY_SPOOL"
}
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
  # The batch endpoint authenticates with the per-device token; a legacy VEHICLE_KEY-only box has no
  # way to send one, so its spool is bounded and left in place rather than posted and refused.
  [ -n "${DEVICE_TOKEN:-}" ] || { spool_cap "$RELAY_SPOOL"; return 0; }
  if [ ! -s "$_sending" ]; then
    spool_cap "$RELAY_SPOOL"
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
  # The status code is read rather than `-f`'s exit status, because a refusal and an outage need
  # opposite handling (classify_http): retrying a 401 forever wedges the spool behind a batch the
  # cloud will never accept, and dropping a 503 loses an alarm to a blip.
  _resp_f="$_sending.resp"
  _code=$(curl -sS --max-time 20 -o "$_resp_f" -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d "$_body" "$_url" 2>/dev/null)
  _resp=$(cat "$_resp_f" 2>/dev/null); rm -f "$_resp_f"
  _verdict=$(classify_http "$_code")
  if [ "$_verdict" = "refused" ]; then
    # Dropped LOUDLY, like the daemon's drain_shelly. The sequence still advances: the cloud did
    # not record this seq, so reusing it would be harmless, but a fresh one keeps the log honest.
    echo "$_seq" > "$RELAY_SEQ_FILE"
    rm -f "$_sending" "$_items_src"
    log "relay: batch seq=$_seq REFUSED by the cloud (HTTP $_code) - dropped; resending cannot help"
    return 0
  fi
  if [ "$_verdict" = "sent" ]; then
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
    # The plan gate travels with the same blob (cloud-server #101): apply it on every reply.
    lt_apply_allowed "$(printf '%s' "$_resp" | lt_parse_allowed)"
  else
    rm -f "$_items_src"
    spool_cap "$_sending"
    log "relay: batch seq=$_seq failed (HTTP ${_code:-000}; will retry with the same seq)"
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

# PURE: one conf line for KEY and VALUE, in a form that is safe to SOURCE. The conf is sourced as root
# by the collector, three CGIs and init.d, so a value that came off the LAN (a name, a GPS password)
# must never be able to become code.
#
# Double quotes when the value is plain, because that is the shape every other writer uses AND the
# shape the package's postinst greps for (`^VID="[^"]+"`): a VID written in single quotes would make
# the next upgrade leave the collector stopped. Anything carrying a shell metacharacter is
# single-quoted instead, with embedded single quotes closed, escaped and reopened.
conf_line() {
  case "$2" in
    *[!A-Za-z0-9\ _.,:/@+=-]*) printf "%s='%s'\n" "$1" "$(printf '%s' "$2" | sed "s/'/'\\\\''/g")" ;;
    *) printf '%s="%s"\n' "$1" "$2" ;;
  esac
}

# Rewrite KEY=VALUE pairs in "$CONF", keeping every other line (unknown keys included) and the mode.
# Args: KEY VALUE [KEY VALUE ...]. Atomic: written beside the conf and moved over it, so a power cut
# mid-write leaves the old file, never half of the new one. Returns 1 when the conf cannot be
# written (a read-only /etc), so a caller never reports a change that did not land.
conf_set() {
  [ $# -ge 2 ] || return 1
  _cs_tmp="${CONF}.$$"
  _cs_keys=""
  _cs_i=1
  for _cs_a in "$@"; do
    [ $((_cs_i % 2)) -eq 1 ] && _cs_keys="${_cs_keys:+$_cs_keys|}$_cs_a"
    _cs_i=$((_cs_i + 1))
  done
  { [ -f "$CONF" ] && grep -vE "^[[:space:]]*(${_cs_keys})=" "$CONF"; } > "$_cs_tmp" 2>/dev/null
  while [ $# -ge 2 ]; do
    conf_line "$1" "$2" >> "$_cs_tmp" || { rm -f "$_cs_tmp"; return 1; }
    shift 2
  done
  chmod 600 "$_cs_tmp" 2>/dev/null
  mv "$_cs_tmp" "$CONF" 2>/dev/null || { rm -f "$_cs_tmp"; return 1; }
}

# PURE: what one HTTP answer means for queued telemetry — the daemon's classify_forward, verbatim.
# 2xx sent; 408, 429 and every 5xx are the path or the cloud being unwell, so keep it and retry; any
# other status is a refusal of THIS payload (bad token, malformed), which resending cannot fix.
# `000` is curl's "no HTTP answer at all" (DNS, no route, timeout) and is a retry.
classify_http() {
  case "$1" in
    2[0-9][0-9]) echo sent ;;
    408|429|5[0-9][0-9]|000|'') echo retry ;;
    *) echo refused ;;
  esac
}

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
#
# Scheme by PORT, exactly as the daemon's gps.rs cradlepoint_base: 443 is HTTPS, anything else is
# HTTP, and an unset port means 443 (owner: "the default should be 443"). NCOS serves a SELF-SIGNED
# certificate on the LAN, so `-k` is required there; the daemon's LAN client accepts invalid certs
# for the same reason (routers.rs lan_client). This is a LAN poll of a box we were told the address
# of, never a WAN call, so there is no CA to check the certificate against in the first place.
cradlepoint_base() {
  _cp_port="${2:-443}"
  [ "$_cp_port" = "0" ] && _cp_port=443
  _cp_scheme=http
  [ "$_cp_port" = "443" ] && _cp_scheme=https
  case "$_cp_port" in
    80|443) printf '%s://%s' "$_cp_scheme" "$1" ;;
    *)      printf '%s://%s:%s' "$_cp_scheme" "$1" "$_cp_port" ;;
  esac
}

read_gps_cradlepoint() {
  [ -n "$CRADLEPOINT_HOST" ] || return 1
  _cp_url="$(cradlepoint_base "$CRADLEPOINT_HOST" "${CRADLEPOINT_PORT:-}")/api/status/gps"
  _cp_k=""
  case "$_cp_url" in https://*) _cp_k="-k" ;; esac
  # shellcheck disable=SC2086
  curl -fsS $_cp_k --max-time 10 -u "${CRADLEPOINT_USER:-admin}:${CRADLEPOINT_PASSWORD:-}" \
    "$_cp_url" 2>/dev/null | parse_cradlepoint_gps
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
  # LinkTap config-as-state, when this reply carries it. Today the worker attaches the blob to
  # /api/agent replies for hub_ devices only and to EVERY batch reply, so on a router it normally
  # arrives through drain_relay; reading it here too costs a substring test and means a cloud that
  # starts sending it on this path needs no hub-lite release to be heard.
  case "$_resp" in
    *'"linktap"'*)
      printf '%s' "$_resp" | lt_parse_profiles | lt_apply_profiles
      lt_apply_allowed "$(printf '%s' "$_resp" | lt_parse_allowed)" ;;
  esac
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
  # "$CONF", not a hard-coded /etc path: the hard-coded one ignored BRVG_HUB_LITE_CONF, so a test (or
  # a Pi install with its conf elsewhere) silently wrote the real router file or nothing at all.
  [ -f "$CONF" ] && conf_set GPS_INTERVAL "$GPS_INTERVAL" MODEM_INTERVAL "$MODEM_INTERVAL"
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
  # The daemon's heartbeat `update`: the newer feed version, when there is one (update_check).
  _upd=$(tr -cd '0-9.' < "$HUB_LITE_UPDATE" 2>/dev/null)
  [ -n "$_upd" ] && _p="$_p&update=$_upd"
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
#
# ⚠️ PLUS THE DAEMON'S SENSOR-FAULT WORDS (linktap_runtime.rs SENSOR_FAULT_WORDS), in the same order.
# The substring rule alone closes the valve on `flood.cable_unplugged` — the real Shelly Flood G4
# event for a probe cable coming loose — because the string contains "flood". A loose cable would
# shut a vessel's water off. A fault is a fault even when its component is the flood sensor.
is_flood_shutoff() {
  _ev=$(printf '%s' "$1" | tr 'A-Z' 'a-z')
  case "$_ev" in
    *.measurement|*.change) return 1 ;;
    *_off|*.off) return 1 ;;
  esac
  case "$_ev" in
    *unplugged*|*disconnected*|*cable*|*fault*|*error*|*low_battery*|*battery_low*|*mute*|*unmute*|*offline*) return 1 ;;
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

# Where every LinkTap fact on this router lives (tmpfs). Per valve: `<dev>` (the running cycle),
# `profile.<dev>`, `ledger.<dev>` and `meas.<dev>` (the last measurement, which /api/hub/linktap/state
# serves); gateway-wide: `unit`, `gw.watch`, `rev`, `wake`. Declared HERE, above the flood close,
# because the close now marks the run it stops and so needs the path too.
#
# 🔴 ONE PATH FOR EVERY WRITER. The /api/hub door used to write its run records to `/tmp/<dev>`
# while this loop read `/tmp/brvg-linktap/<dev>`, so a washdown opened through the door was met
# by the next poll as an unknown running valve and ADOPTED as a Normal Run on the profile cap — the
# washdown volume-cut the door's own comment said it prevented. Only a second bug (the door's key
# check read a variable nothing set) kept that from ever happening.
LT_STATE_DIR="${LT_STATE_DIR:-/tmp/brvg-linktap}"

# PURE: the canonical 16-character valve id, the TS client's normalizeDevId and the daemon's.
lt_norm_id() { printf '%s' "$1" | tr -cd 'A-Za-z0-9' | cut -c1-16; }

# Is a gateway configured at all? Every LinkTap path is a strict no-op without all three.
lt_configured() { [ -n "${LINKTAP_HOST:-}" ] && [ -n "${LINKTAP_GW_ID:-}" ] && [ -n "${LINKTAP_DEV_IDS:-}" ]; }

# Is $1 (already normalised) one of the configured valves? A hub is not a general-purpose proxy onto
# the vessel's RF network.
lt_is_watched() {
  for _iw in $(printf '%s' "${LINKTAP_DEV_IDS:-}" | tr ',' ' '); do
    [ "$(lt_norm_id "$_iw")" = "$1" ] && return 0
  done
  return 1
}

# POST one command body to the gateway; the reply on stdout. $2 = timeout seconds (default 10).
lt_post() {
  curl -fsS --max-time "${2:-10}" -X POST -H 'Content-Type: application/json' -d "$1" \
    "http://${LINKTAP_HOST}/api.shtml" 2>/dev/null
}

# Append one line to the relay spool. BRVG_RELAY_SPOOL is read at CALL time, because the receiver
# CGI and the tests set it per call.
lt_spool() {
  printf '%s\t%s\t%s\t%s\n' "$(date +%s)" "$1" "$2" "$3" >> "${BRVG_RELAY_SPOOL:-$RELAY_SPOOL}"
}

# Ring the poll loop (the daemon's linktap_wake): a valve command or a gateway push asks for a read
# NOW, so what the gateway did reaches the cloud within seconds rather than on the next poll.
# Only where the state dir already exists — the loop creates it on its first poll, and a flood
# close on a box that has never polled has nothing to read back.
lt_wake() { [ -d "$LT_STATE_DIR" ] && : > "$LT_STATE_DIR/wake"; return 0; }

# Record THIS run's facts atomically. $1 file, then: state started stop mode dur cap prov resume
# handover. Temp-and-move, so the poll (a separate process from the door) never reads half a record.
lt_write_state() {
  _wsf="$1.$$"
  printf 'state=%s\nstarted=%s\nstop=%s\nmode=%s\ndur=%s\ncap=%s\nprov=%s\nresume=%s\nhandover=%s\n' \
    "$2" "$3" "$4" "$5" "$6" "$7" "$8" "${9:-0}" "${10:-0}" > "$_wsf" 2>/dev/null && mv "$_wsf" "$1" 2>/dev/null
  rm -f "$_wsf" 2>/dev/null
  return 0
}

# Mark a stop WE issued on a running cycle, keeping everything else about it — the daemon's
# note_stop. Without it a flood close or a manual close classifies as `unknown` when the valve
# shuts, and the cycle's end tells the owner nothing about why. No file means no running cycle:
# nothing to mark, exactly like note_stop on an idle track.
lt_mark_stop() {
  [ -f "$1" ] || return 0
  lt_load_state "$1" 0 0
  [ "$_state" = "watering" ] || return 0
  lt_write_state "$1" watering "$_started" "$2" "$_mode" "$_dur_eff" "$_cap_eff" "$_prov" "$_resume" "$_handover"
}

# Close every valve in $LINKTAP_DEV_IDS via http://$LINKTAP_HOST/api.shtml. No-op when LinkTap is
# not configured, so every existing install is untouched. Each attempt spools a
# linktap.flood_close.change line (rides the roll-up — visibility with zero new wire surface) and
# logs locally; a failed close is spooled with ok=0 rather than retried here — the alarm itself is
# already on its way to the cloud, and the worker's own flood path remains the escalation.
#
# ⚠️ NEVER PLAN-GATED, deliberately, exactly as the daemon's linktap_flood_stop_all: a close spends
# no water, removes no limit and is idempotent, and the worst outcome of running it on a vehicle
# whose plan does not include valve control is a boat that did not flood. LINKTAP_ALLOWED is not
# read anywhere on this path.
#
# Each close also marks the run `stop=flood_shutoff` (so the end classifies as flood_shutoff and a
# washdown told to resume can never reopen), spools `linktap.stop_failed` with cause=flood when the
# gateway did not take it (the daemon's event), and rings the poll loop to read the result back.
linktap_flood_close() {
  lt_configured || return 0
  for _d in $(printf '%s' "$LINKTAP_DEV_IDS" | tr ',' ' '); do
    _d=$(lt_norm_id "$_d")
    [ -n "$_d" ] || continue
    # Marked BEFORE the command, like the daemon: if the stop lands and this process dies before
    # it can write, the end must still read as ours rather than as an unexplained close.
    lt_mark_stop "$LT_STATE_DIR/$_d" flood_shutoff
    if lt_post "$(linktap_stop_body "$LINKTAP_GW_ID" "$_d")" 5 >/dev/null; then
      _ok=1
    else
      _ok=0
    fi
    lt_spool "lt_${_d}" "linktap.flood_close.change" "ok=${_ok}"
    [ "$_ok" = "1" ] || lt_spool "lt_${_d}" "linktap.stop_failed" "error=gateway_unreachable&cause=flood"
    logger -t brvg-hub-lite "flood shutoff: valve ${_d} close ok=${_ok}" 2>/dev/null || true
  done
  lt_wake
}

# --- LinkTap: cycle semantics on hub-lite (parity port of hub/src/cycle.ts) ---------------------
# Owner doctrine 2026-08-19: "hub lite should do anything a hub can do as long as it is not
# CPU/memory restrictive." This is the schedules port: the SAME decision table as the TypeScript
# cycle machine — the shared fixtures in test.sh mirror test/cycle.test.ts case for case, which is
# what keeps two implementations from diverging (the one-contract rule).
#
# Scope (0.15.0): the whole cycle machine — normal runs, washdown and tank fill through the door,
# the software volume cutoff, end classification, restart-only-on-timer, the washdown handover and
# resume, the ledger and adoption of external opens. State lives in tmpfs — a reboot loses it and
# the ADOPTION rule rebuilds it from the gateway's own answer, exactly like the daemon's restart rule.

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
      # "-" when the gateway gave no remaining time, never an empty field: the caller splits this
      # line on whitespace, and an empty third field silently shifted SPEED into the remain slot.
      rem = "-"
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

# cmd 6 body — duration SECONDS, volume_limit in the GATEWAY unit ($4 already converted).
#
# An empty or zero cap OMITS volume_limit rather than sending 0, as the daemon's linktap::build_start
# does: a washdown is time-only by owner spec, and "volume_limit":0 is a number the gateway firmware
# is free to read as something. Absent is unambiguous.
lt_start_body() {
  if [ -n "$4" ] && awk -v c="$4" 'BEGIN{exit !(c > 0)}'; then
    printf '{"cmd":6,"gw_id":"%s","dev_id":"%s","duration":%d,"volume_limit":%s}' "$1" "$2" "$3" "$4"
  else
    printf '{"cmd":6,"gw_id":"%s","dev_id":"%s","duration":%d}' "$1" "$2" "$3"
  fi
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
      # 🔴 NOT "profiles" IMMEDIATELY AFTER THE BRACE. The worker builds the blob as
      # `{ allowed, ...profiles }`, so the real reply is {"linktap":{"allowed":true,"profiles":{..}}}
      # and the old anchor matched NOTHING a real worker has ever sent: every wire profile was
      # dropped and every valve ran on the conf default. [^{}]* skips the scalar siblings.
      if (!match(buf, /"linktap":[[:space:]]*\{[^{}]*"profiles":[[:space:]]*\{/)) exit
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

# The plan gate out of a worker reply: prints 1 or 0 for `"linktap":{..."allowed":true|false...}`,
# nothing when the reply carries no LinkTap blob (the common case — which must NOT read as a
# revocation). Depth-aware, so an "allowed" inside a nested profile object can never be taken for
# the vehicle's permission.
lt_parse_allowed() {
  awk '
    { buf = buf $0 }
    END {
      i = index(buf, "\"linktap\""); if (i == 0) exit
      rest = substr(buf, i + 9)
      if (!match(rest, /^[[:space:]]*:[[:space:]]*\{/)) exit
      rest = substr(rest, RLENGTH)
      depth = 0; instr = 0; n = length(rest)
      for (j = 1; j <= n; j++) {
        c = substr(rest, j, 1)
        if (instr) { if (c == "\\") j++; else if (c == "\"") instr = 0; continue }
        if (c == "{") { depth++; continue }
        if (c == "}") { depth--; if (depth == 0) exit; continue }
        if (c == "\"") {
          if (depth == 1 && substr(rest, j, 9) == "\"allowed\"") {
            tail = substr(rest, j + 9)
            if (match(tail, /^[[:space:]]*:[[:space:]]*true/)) { print 1; exit }
            if (match(tail, /^[[:space:]]*:[[:space:]]*false/)) { print 0; exit }
          }
          instr = 1
        }
      }
    }'
}

# Adopt the cloud's answer on valve control. Persisted to the conf ONLY WHEN IT CHANGES — a flash
# write per plan change, not per report — so a router that boots with no WAN still knows the last
# answer, the same reason the daemon keeps `linktap.allowed` in hub.json. Defaults to 0 everywhere:
# a hub-lite that has never heard from the cloud may not OPEN a valve. Empty input is "no blob in
# this reply" and changes nothing.
lt_apply_allowed() {
  case "$1" in 0|1) : ;; *) return 0 ;; esac
  [ "$1" = "${LINKTAP_ALLOWED:-0}" ] && return 0
  LINKTAP_ALLOWED="$1"
  [ -f "$CONF" ] && conf_set LINKTAP_ALLOWED "$1"
  if [ "$1" = "1" ]; then
    log "linktap: valve control permitted by the vehicle's plan"
  else
    log "linktap: valve control NOT permitted by the vehicle's plan - opens refused (closes never are)"
  fi
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
#
# Also sets _prov (hub | adopted), _resume and _handover (0/1). A file with no prov predates 0.15
# and was written by adoption or by an auto-restart; `adopted` is the honest reading of the two,
# because it claims nothing about targets and can never trigger a handover (which needs `hub`).
lt_load_state() {
  state=""; started=""; stop=""; mode=""; dur=""; cap=""; prov=""; resume=""; handover=""
  # shellcheck disable=SC1090
  [ -f "$1" ] && . "$1"
  _prov="${prov:-adopted}"
  _resume="${resume:-0}"
  _handover="${handover:-0}"
  _state="${state:-idle}"
  _started="${started:-0}"
  _stop="${stop:-}"
  # This run's own targets win over the profile's; absent means "a Normal Run on the profile",
  # which is exactly what a state file written before 2026-09-01 meant.
  _mode="${mode:-normal}"
  _dur_eff="${dur:-$2}"
  _cap_eff="${cap:-$3}"
}

# --- LinkTap: the rest of the daemon's runtime (0.15.0 parity) -----------------------------------
# linktap_runtime.rs, cycle.rs and hub_server.rs's poll loop, in shell. Every decision is a PURE
# function below with its fixtures in test.sh, ported from the daemon's own tests case for case
# (the one-contract rule); linktap_tick is only the I/O that strings them together.

# How long BEFORE a resumable washdown expires the valve is reprogrammed into its Normal Run —
# the daemon's cycle::HANDOVER_LEAD_SECS. Owner, MVP 2026-08-31: "would be nice if it reprogrammed
# it right before it was going to stop, so it never stops the water flow". Waiting for the close and
# THEN reopening was measured at 30 s of dry pipe; cmd 6 on an already-open valve swaps the plan
# underneath a valve that never shuts.
LT_HANDOVER_LEAD_SECS=20

# The grace window before an unreachable gateway is worth telling anyone about — the daemon's
# GATEWAY_OFFLINE_GRACE_SECS, and the cloud's GATEWAY_OFFLINE_GRACE_MS. Gateways FLAP; the owner set
# "30 min plus", and the two debounces must not disagree about what counts as an outage.
LT_GATEWAY_GRACE_SECS=1800

# PURE: should this RUNNING cycle be reprogrammed into its Normal Run now? cycle.rs should_hand_over.
#   $1 mode  $2 resume(0/1)  $3 prov  $4 stop ("" = none)  $5 handover(0/1)  $6 remain ("-"/"" =
#   unknown)  $7 started  $8 dur  $9 now  [$10 lead, default LT_HANDOVER_LEAD_SECS]
# ONLY a hub-issued washdown that was told to resume, and only once. A washdown with ANY stop issued
# — flood, manual, volume cap — is on its way shut on purpose and must never be handed over.
lt_should_hand_over() {
  [ "$5" = "1" ] && return 1
  [ -n "$4" ] && return 1
  [ "$1" = "washdown" ] && [ "$2" = "1" ] || return 1
  [ "$3" = "hub" ] || return 1
  # The GATEWAY's own remaining time when it offers one (the clock the valve actually stops on);
  # our own arithmetic otherwise, so a non-reporting gateway still hands over.
  case "$6" in
    ''|-|0|*[!0-9]*)
      _ho_el=$(( $9 - $7 )); [ "$_ho_el" -lt 0 ] && _ho_el=0
      _ho_left=$(( $8 - _ho_el )); [ "$_ho_left" -lt 0 ] && _ho_left=0 ;;
    *) _ho_left=$6 ;;
  esac
  [ "$_ho_left" -le "${10:-$LT_HANDOVER_LEAD_SECS}" ]
}

# PURE: should this END be followed by the Normal Run the washdown was told to resume?
# cycle.rs should_resume_normal. TIMER ONLY: a washdown cut short by a flood, a manual stop or an
# unexplained close must never reopen the valve. "When unsure, spend no water."
#   $1 mode  $2 reason  $3 resume(0/1)
lt_should_resume() {
  [ "$1" = "washdown" ] && [ "$2" = "timer" ] && [ "$3" = "1" ]
}

# PURE: seconds until the poll loop MUST look again, or nothing when nothing is time-critical —
# linktap_runtime.rs poll_hint. A resumable washdown needs a poll INSIDE its 20 s lead window, and
# the standing cadence is wider than the window, so a fixed interval could step straight over it.
# Floored at 5 s so a valve seconds from handover cannot spin the gateway. $1 = now.
lt_poll_hint() {
  _ph_best=""
  for _ph_f in "$LT_STATE_DIR"/*; do
    [ -f "$_ph_f" ] || continue
    case "${_ph_f##*/}" in *.*|unit|wake|rev) continue ;; esac
    lt_load_state "$_ph_f" 0 0
    [ "$_state" = "watering" ] || continue
    [ -z "$_stop" ] && [ "$_handover" != "1" ] && [ "$_mode" = "washdown" ] && [ "$_resume" = "1" ] && [ "$_prov" = "hub" ] || continue
    _ph_el=$(( $1 - _started )); [ "$_ph_el" -lt 0 ] && _ph_el=0
    _ph_left=$(( _dur_eff - _ph_el )); [ "$_ph_left" -lt 0 ] && _ph_left=0
    _ph_until=$(( _ph_left - LT_HANDOVER_LEAD_SECS )); [ "$_ph_until" -lt 0 ] && _ph_until=0
    if [ -z "$_ph_best" ] || [ "$_ph_until" -lt "$_ph_best" ]; then _ph_best=$_ph_until; fi
  done
  [ -n "$_ph_best" ] || return 0
  [ "$_ph_best" -lt 5 ] && _ph_best=5
  echo "$_ph_best"
}

# PURE: fold one poll's outcome into the gateway watch — linktap_runtime.rs gateway_watch_step.
#   $1 last_seen epoch ("" = never)  $2 offline_reported(0/1)  $3 reached(0/1)  $4 now  [$5 grace]
# Prints "<last_seen> <reported> <none|offline|online> <mins>". OFFLINE once per episode and only
# after the whole grace window; ONLINE only when an offline was reported, because a recovery notice
# for an outage nobody was told about is exactly the flap the window exists to swallow.
lt_gw_watch_step() {
  _gw_grace="${5:-$LT_GATEWAY_GRACE_SECS}"
  if [ "$3" = "1" ]; then
    _gw_mins=0
    [ -n "$1" ] && _gw_mins=$(( ($4 - $1) / 60 )) && [ "$_gw_mins" -lt 0 ] && _gw_mins=0
    if [ "$2" = "1" ]; then echo "$4 0 online $_gw_mins"; else echo "$4 0 none 0"; fi
    return 0
  fi
  # A hub-lite that has NEVER seen this gateway answer starts its clock now, rather than claiming an
  # outage of unknown length.
  [ -n "$1" ] || { echo "$4 0 none 0"; return 0; }
  [ "$2" = "1" ] && { echo "$1 1 none 0"; return 0; }
  [ $(( $4 - $1 )) -lt "$_gw_grace" ] && { echo "$1 0 none 0"; return 0; }
  echo "$1 1 offline $(( ($4 - $1) / 60 ))"
}

# PURE: the valve ids a gateway HTTP push names, normalised, one per line — linktap_runtime.rs
# parse_gateway_push, for both shapes (`dev_stat:[{dev_id..}]` and a bare `{dev_id..}`), HTML-wrapped
# or not. Junk yields nothing.
lt_parse_push() {
  tr -d '\n\r' | grep -o '"dev_id"[[:space:]]*:[[:space:]]*"[A-Za-z0-9]*"' | sed 's/.*"\([A-Za-z0-9]*\)"$/\1/' \
    | while IFS= read -r _pp; do _pp=$(lt_norm_id "$_pp"); [ -n "$_pp" ] && echo "$_pp"; done
}

# PURE: the valve's own health off a cmd 3 reply, as measurement params — what the app's valve view
# reads (hubValveReading.ts) and the daemon's observe() reports: meters, battery, signal, rf, and the
# fault flags broken/leak/clog/cutoff. Printed as "&k=v&k=v". A field the gateway OMITS is omitted
# here too — a missing reading is not a flat battery.
lt_parse_fields() {
  awk '
    { buf = buf $0 }
    function num(k,  v) {
      if (!match(buf, "\"" k "\":[[:space:]]*-?[0-9.]+")) return ""
      v = substr(buf, RSTART, RLENGTH); sub(/.*:[[:space:]]*/, "", v); v += 0
      return (v < 0) ? int(v - 0.5) : int(v + 0.5)
    }
    function flag(k,  v) {
      if (!match(buf, "\"" k "\":[[:space:]]*(true|false|\"true\"|\"false\"|\"1\"|\"0\"|-?[0-9.]+)")) return ""
      v = substr(buf, RSTART, RLENGTH); sub(/.*:[[:space:]]*/, "", v); gsub(/"/, "", v)
      if (v == "true") return 1
      if (v == "false" || v == "") return 0
      return (v + 0 != 0) ? 1 : 0
    }
    END {
      # is_flm_plugin is authoritative when present (as a JSON boolean); otherwise a volume field
      # means the valve meters — linktap::reports_volume.
      m = ""
      if (match(buf, /"is_flm_plugin":[[:space:]]*(true|false)/)) { v = substr(buf, RSTART, RLENGTH); m = (v ~ /true/) ? 1 : 0 }
      else m = (buf ~ /"volume":[[:space:]]*-?[0-9.]+/) ? 1 : 0
      out = "&meters=" m
      b = num("battery"); if (b != "") out = out "&battery=" b
      g = num("signal");  if (g != "") out = out "&signal=" g
      r = flag("is_rf_linked"); if (r != "") out = out "&rf=" r
      split("is_broken broken is_leak leak is_clog clog is_cutoff cutoff", t, " ")
      for (i = 1; i <= 8; i += 2) { f = flag(t[i]); if (f != "") out = out "&" t[i + 1] "=" f }
      printf "%s", out
    }'
}

# The effective profile for one valve: the wire profile's fields over the conf defaults, FIELD BY
# FIELD (the profileFor rule). Sets _p_dur _p_cap _p_ar.
lt_profile() {
  _p_dur="${LINKTAP_NORMAL_SECS:-86400}"
  _p_cap="${LINKTAP_NORMAL_VOL_L:-378}"
  _p_ar="${LINKTAP_AUTO_RESTART:-0}"
  if [ -f "$LT_STATE_DIR/profile.$1" ]; then
    P_DUR=""; P_VOL=""; P_AR=""
    # shellcheck disable=SC1090
    . "$LT_STATE_DIR/profile.$1"
    [ -n "$P_DUR" ] && _p_dur="$P_DUR"
    [ -n "$P_VOL" ] && _p_cap="$P_VOL"
    [ -n "$P_AR" ]  && _p_ar="$P_AR"
  fi
  return 0
}

# The gateway's volume unit, read ONCE and cached (a config change needs a gateway visit anyway).
# Defaults to GALLONS when unreadable — guessing litres under-reports a cap 3.79x, and the software
# cutoff compares against that number (daemon read_vol_unit).
lt_unit() {
  if [ -s "$LT_STATE_DIR/unit" ]; then cat "$LT_STATE_DIR/unit"; return 0; fi
  _lu=$(lt_post "{\"cmd\":16,\"gw_id\":\"$LINKTAP_GW_ID\"}" 10 | grep -o '"vol_unit":"[^"]*"' | cut -d'"' -f4)
  [ "$_lu" = "L" ] || _lu="gal"
  mkdir -p "$LT_STATE_DIR" 2>/dev/null && echo "$_lu" > "$LT_STATE_DIR/unit"
  echo "$_lu"
}

# May this router OPEN a valve? Only when the cloud has said the vehicle's plan permits it — the
# daemon's `linktap.allowed`, default DENY. CLOSING is never asked this question anywhere.
lt_open_allowed() { [ "${LINKTAP_ALLOWED:-0}" = "1" ]; }

# Open one valve and record the run as OURS. $1 dev  $2 duration secs  $3 volume_limit to SEND in
# litres ("" = none)  $4 cap litres to TRACK (0 for a washdown)  $5 mode  $6 resume(0/1).
# Returns non-zero when the gateway did not take it, having recorded nothing.
#
# Recording is what stops the next poll ADOPTING our own run (the daemon's note_hub_open): an
# adopted run takes the profile's cap, which for a washdown is exactly the cap that must not exist.
lt_open() {
  _o_unit=$(lt_unit)
  _o_gw=""
  [ -n "$3" ] && _o_gw=$(awk -v c="$3" -v u="$_o_unit" 'BEGIN{ if (c + 0 > 0) printf "%.2f", (u == "gal") ? c / 3.785411784 : c }')
  lt_post "$(lt_start_body "$LINKTAP_GW_ID" "$1" "$2" "$_o_gw")" 10 >/dev/null || return 1
  _o_res=0
  [ "$5" = "washdown" ] && [ "$6" = "1" ] && _o_res=1
  mkdir -p "$LT_STATE_DIR" 2>/dev/null
  lt_write_state "$LT_STATE_DIR/$1" watering "$(date +%s)" "" "$5" "$2" "$4" hub "$_o_res" 0
  lt_wake
  return 0
}

# PURE: the linktap.measurement params, in the daemon's order and formats (linktap_runtime.rs
# observe). $1 watering  $2 vol_l  $3 fields (lt_parse_fields)  $4 speed L/min  $5 ledger fragment
# ("&day=..&day_vol_l=..", or "")  $6 running(0/1)  $7 mode  $8 dur  $9 cap  $10 remain ("-" = none)
# $11 prov. The run's targets ride only WHILE RUNNING, and the flow rate only while watering: a
# finished run's numbers or the last non-zero rate, carried forward, are drawn by the app forever.
lt_measurement_params() {
  _mp="watering=$1&vol_l=$(awk -v v="$2" 'BEGIN{printf "%.2f", v}')$3"
  [ "$1" = "1" ] && _mp="$_mp&flow_lpm=$(awk -v v="${4:-0}" 'BEGIN{printf "%.2f", v}')"
  _mp="$_mp$5"
  if [ "$6" = "1" ]; then
    _mp="$_mp&mode=$7&dur_s=$8&cap_l=$(awk -v v="${9:-0}" 'BEGIN{printf "%.2f", v}')"
    case "${10}" in ''|-|0|*[!0-9]*) : ;; *) _mp="$_mp&remain_s=${10}" ;; esac
    _mp="$_mp&prov=${11}"
  fi
  printf '%s' "$_mp"
}

# Bump the valve-state revision the /api/hub/linktap/state door hands back as `rev`.
lt_bump_rev() {
  _rv=$( (cat "$LT_STATE_DIR/rev" 2>/dev/null || echo 0) | tr -cd '0-9')
  echo $(( ${_rv:-0} + 1 )) > "$LT_STATE_DIR/rev"
}

# One poll pass over every configured valve: observe, act, report — linktap_poll_loop + observe +
# linktap_act. State per valve in $LT_STATE_DIR/<dev>:
#   state=watering started=<epoch> stop= |volume_cap|manual|flood_shutoff
#   mode=normal|washdown|tankfill dur=<secs> cap=<litres> prov=hub|adopted resume=0|1 handover=0|1
# No file = idle. Every field describes THIS RUN, not the profile.
linktap_tick() {
  lt_configured || return 0
  mkdir -p "$LT_STATE_DIR" 2>/dev/null
  rm -f "$LT_STATE_DIR/wake" 2>/dev/null
  _unit=$(lt_unit)
  _lt_reached=""

  for _d in $(printf '%s' "$LINKTAP_DEV_IDS" | tr ',' ' '); do
    _d=$(lt_norm_id "$_d")
    [ -n "$_d" ] || continue
    lt_profile "$_d"
    # A reply at all is "the gateway answered", whatever it said about this valve: a `ret: 5` on one
    # valve is a flat battery, not an outage (daemon reply_reached_gateway).
    if ! _reply=$(lt_post "{\"cmd\":3,\"gw_id\":\"$LINKTAP_GW_ID\",\"dev_id\":\"$_d\"}" 10); then
      _lt_reached="${_lt_reached:-0}"
      continue
    fi
    _lt_reached=1
    # shellcheck disable=SC2046
    set -- $(printf '%s' "$_reply" | lt_parse_status "$_unit")
    _w="${1:-0}"; _volL="${2:-0}"; _rem="${3:--}"; _speedL="${4:-0}"
    _fields=$(printf '%s' "$_reply" | lt_parse_fields)

    _sf="$LT_STATE_DIR/$_d"
    lt_load_state "$_sf" "$_p_dur" "$_p_cap"
    _now=$(date +%s)
    _elapsed=$(( _now - _started ))
    _reopen=""

    _act=$(lt_decide "$_state" "$_w" "$_volL" "$_cap_eff" "$_stop" "$_elapsed" "$_dur_eff" "$_speedL")
    case "$_act" in
      adopt)
        # Manual press / external open IS a Normal Run with the profile cap (owner rule), bounded by
        # what the GATEWAY says is left when it says anything (cycle.rs adopt_cycle).
        _adur="$_p_dur"
        case "$_rem" in ''|-|0|*[!0-9]*) : ;; *) _adur="$_rem" ;; esac
        lt_write_state "$_sf" watering "$_now" "" normal "$_adur" "$_p_cap" adopted 0 0
        logger -t brvg-hub-lite "linktap: adopted a running cycle on ${_d} (Normal Run cap ${_p_cap}L)" 2>/dev/null || true
        ;;
      cut)
        # Marked first, so the close that follows classifies against the run that was running.
        lt_write_state "$_sf" watering "$_started" volume_cap "$_mode" "$_dur_eff" "$_cap_eff" "$_prov" "$_resume" "$_handover"
        if lt_post "$(linktap_stop_body "$LINKTAP_GW_ID" "$_d")" 5 >/dev/null; then
          logger -t brvg-hub-lite "linktap: volume cap ${_cap_eff}L reached on ${_d} - stop issued" 2>/dev/null || true
        else
          # A close that did not happen is worth hearing about now (daemon linktap.stop_failed).
          lt_spool "lt_${_d}" "linktap.stop_failed" "error=gateway_unreachable"
          logger -t brvg-hub-lite "linktap: ${_d} STOP FAILED - will re-issue on the next poll" 2>/dev/null || true
          # 🔴 AND THE MARK IS TAKEN BACK. A stop that never reached the gateway left `stop=volume_cap`
          # set, and lt_decide never cuts a run that already has a stop — so one lost packet meant
          # the only volume enforcement there is stayed off for the rest of the run. Un-marked, the
          # next poll cuts again: one attempt per poll, never a storm.
          lt_write_state "$_sf" watering "$_started" "" "$_mode" "$_dur_eff" "$_cap_eff" "$_prov" "$_resume" "$_handover"
        fi
        ;;
      ended:*)
        _reason="${_act#ended:}"
        rm -f "$_sf"
        # Washdown does NOT count against the day (owner rule, daemon apply_to_ledger).
        _dayvol=$(lt_ledger_apply "$_mode" "$_volL" "$(lt_day_key)" "$LT_STATE_DIR/ledger.$_d")
        lt_spool "lt_${_d}" "linktap.cycle.change" \
          "mode=${_mode}&reason=${_reason}&vol_l=$(awk -v v="$_volL" 'BEGIN{printf "%.2f", v}')&day=$(lt_day_key)&day_vol_l=${_dayvol}"
        # Resume is checked FIRST: it answers an instruction attached to THAT run, where auto-restart
        # is a standing profile switch (linktap_runtime.rs observe).
        if lt_should_resume "$_mode" "$_reason" "$_resume"; then
          _reopen="washdown resume"
        elif lt_should_restart "$_reason" "$_p_ar" "$_mode"; then
          _reopen="auto-restart"
        fi
        ;;
      none) : ;;
    esac

    # 🔴 THE SEAMLESS HANDOVER, decided while the valve is STILL OPEN, exactly once.
    if [ -f "$_sf" ]; then
      lt_load_state "$_sf" "$_p_dur" "$_p_cap"
      if lt_should_hand_over "$_mode" "$_resume" "$_prov" "$_stop" "$_handover" "$_rem" "$_started" "$_dur_eff" "$_now"; then
        lt_write_state "$_sf" watering "$_started" "" "$_mode" "$_dur_eff" "$_cap_eff" "$_prov" "$_resume" 1
        _reopen="washdown handover"
      fi
    fi

    # Telemetry rides the roll-up, with the same event name and params as the daemon, so the cloud
    # cannot tell the tiers apart. The running cycle is read back AFTER the decision, like observe().
    _ldg=""
    if [ -f "$LT_STATE_DIR/ledger.$_d" ]; then
      DAY=""; DAY_VOL=0
      # shellcheck disable=SC1090
      . "$LT_STATE_DIR/ledger.$_d"
      _ldg="&day=${DAY}&day_vol_l=$(awk -v v="$DAY_VOL" 'BEGIN{printf "%.2f", v}')"
    fi
    _run=0
    if [ -f "$_sf" ]; then lt_load_state "$_sf" "$_p_dur" "$_p_cap"; _run=1; fi
    _meas=$(lt_measurement_params "$_w" "$_volL" "$_fields" "$_speedL" "$_ldg" "$_run" "$_mode" "$_dur_eff" "$_cap_eff" "$_rem" "$_prov")
    lt_spool "lt_${_d}" "linktap.measurement" "$_meas"
    printf '%s\n' "$_meas" > "$LT_STATE_DIR/meas.$_d.$$" && mv "$LT_STATE_DIR/meas.$_d.$$" "$LT_STATE_DIR/meas.$_d"

    # THE REOPEN, performed after the report like the daemon's linktap_act. It always returns the
    # valve to its PROFILE's Normal Run. Plan-gated: an open is the paid feature.
    if [ -n "$_reopen" ]; then
      if ! lt_open_allowed; then
        log "linktap: ${_d} - ${_reopen} skipped: the vehicle's plan does not permit opening a valve"
        lt_spool "lt_${_d}" "linktap.reopen_failed" "why=$(urlencode_spaces "$_reopen")&error=plan_not_permitted"
      elif lt_open "$_d" "$_p_dur" "$_p_cap" "$_p_cap" normal 0; then
        logger -t brvg-hub-lite "linktap: ${_d} - ${_reopen} -> reopened for ${_p_dur}s" 2>/dev/null || true
      else
        lt_spool "lt_${_d}" "linktap.reopen_failed" "why=$(urlencode_spaces "$_reopen")&error=gateway_unreachable"
        logger -t brvg-hub-lite "linktap: ${_d} - ${_reopen} FAILED" 2>/dev/null || true
      fi
    fi
  done

  # The GATEWAY-REACHABILITY WATCH. This loop is the only thing aboard that talks to the gateway, so
  # it is the only thing that can tell an unreachable gateway from a quiet one.
  if [ -n "$_lt_reached" ]; then
    set -- $(cat "$LT_STATE_DIR/gw.watch" 2>/dev/null)
    _gws=$(lt_gw_watch_step "${1:-}" "${2:-0}" "$_lt_reached" "$(date +%s)")
    set -- $_gws
    echo "$1 $2" > "$LT_STATE_DIR/gw.watch"
    if [ "$3" != "none" ]; then
      lt_spool "lt_gw_${LINKTAP_GW_ID}" "linktap.gateway.$3" "host=${LINKTAP_HOST}&mins=$4"
      log "linktap: gateway ${LINKTAP_HOST} - linktap.gateway.$3"
    fi
  fi
  lt_bump_rev
  spool_cap "${BRVG_RELAY_SPOOL:-$RELAY_SPOOL}"
}

# --- Update visibility (the daemon's update_check_loop, phase 1a) --------------------------------
# Visibility only: WHAT is installed is still decided by the signed feed and the argument-free
# self_update. This only tells the owner (status `updateAvailable`, and `update=` on the modem
# report the fleet console reads) that a newer hub-lite exists.
#
# The daemon asks GitHub for its latest tag; a hub-lite asks the feed it would actually update from,
# so "update available" can never name a version self_update cannot install. The index is a few
# hundred bytes; `opkg update` would also refresh every OpenWrt feed on a metered link, so it is not
# used for looking. Every 6 hours, like the daemon.
UPDATE_CHECK_SECS=21600

# PURE: is dotted version $1 strictly newer than $2? Numeric per component; a missing component is 0.
version_newer() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    na = split(a, x, "."); nb = split(b, y, "."); n = (na > nb) ? na : nb
    for (i = 1; i <= n; i++) { p = x[i] + 0; q = y[i] + 0; if (p > q) exit 0; if (p < q) exit 1 }
    exit 1
  }'
}

# PURE: the brvg-hub-lite Version out of an opkg Packages index on stdin.
feed_version() {
  awk '/^Package:/ { pkg = $2 } /^Version:/ && pkg == "brvg-hub-lite" { print $2; exit }'
}

update_check() {
  _uc_feed=$(sed -n 's/^src\/gz[[:space:]][[:space:]]*brvg_hublite[[:space:]][[:space:]]*\([^[:space:]]*\).*/\1/p' \
    /etc/opkg/customfeeds.conf 2>/dev/null | tail -1)
  [ -n "$_uc_feed" ] || return 0
  _uc_v=$(curl -fsSL --max-time 20 "$_uc_feed/Packages" 2>/dev/null | feed_version)
  [ -n "$_uc_v" ] || return 0
  if version_newer "$_uc_v" "$HUB_LITE_VERSION"; then
    [ "$(cat "$HUB_LITE_UPDATE" 2>/dev/null)" = "$_uc_v" ] || log "update available: $_uc_v (running $HUB_LITE_VERSION)"
    echo "$_uc_v" > "$HUB_LITE_UPDATE"
  else
    rm -f "$HUB_LITE_UPDATE" 2>/dev/null
  fi
}

# --- Main loop ---------------------------------------------------------------------------------

# PURE: how long to sleep before the next piece of due work. $1 now, then due epochs (empty = none).
# Never below 1 s (a due time in the past means "loop straight round"), and never above
# $LT_NAP_SLICE when it is set — 5 s while a valve could be woken, so a valve command or a gateway
# push (which ring the wake file) is noticed within seconds rather than a whole GPS interval later.
next_nap() {
  _nn_now=$1; shift
  _nn_best=""
  for _nn in "$@"; do
    [ -n "$_nn" ] || continue
    _nn_d=$(( _nn - _nn_now ))
    if [ -z "$_nn_best" ] || [ "$_nn_d" -lt "$_nn_best" ]; then _nn_best=$_nn_d; fi
  done
  [ -n "$_nn_best" ] || _nn_best=60
  [ "$_nn_best" -lt 1 ] && _nn_best=1
  [ -n "${LT_NAP_SLICE:-}" ] && [ "$_nn_best" -gt "$LT_NAP_SLICE" ] && _nn_best=$LT_NAP_SLICE
  echo "$_nn_best"
}

main() {
  # The first start after boot or restart opens the first-run window (init.d normally wrote it).
  [ -s "$HUB_LITE_STARTED" ] || date +%s > "$HUB_LITE_STARTED" 2>/dev/null
  # 🔴 AN UNCLAIMED BOX WAITS, IT DOES NOT CRASH-LOOP. With no VID (a fresh .ipk, or /api/hub/clear)
  # load_config exits, procd respawns it every 30 s forever, and the log fills with the same fatal
  # line. Wait for /api/hub/bootstrap to write one instead.
  while :; do
    # shellcheck disable=SC1090
    VID=""; DEVICE_ID=""; [ -f "$CONF" ] && . "$CONF"
    [ -n "$VID" ] && [ -n "$DEVICE_ID" ] && break
    [ -n "${_waited:-}" ] || log "no vehicle configured yet - waiting for setup (/api/hub/bootstrap)"
    _waited=1
    sleep 30
  done
  load_config
  log "starting (platform=$(detect_platform), gps every ${GPS_INTERVAL}s, modem every ${MODEM_INTERVAL}s)"
  # Small random start offset so a fleet doesn't tick in lockstep after a regional power event.
  sleep $(( $$ % 20 ))
  # An urgent webhook (alarm) pokes the drain immediately — aggregation must never delay one that
  # the CGI failed to deliver directly. Not gated on HUB_LITE_ENABLED any more: see drain below.
  trap drain_relay USR1
  _next_gps=0; _next_modem=0; _next_lt=0; _next_update=0
  while :; do
    _now=$(date +%s)
    # A CGI rewrote the conf (/api/hub/config, /token, /bootstrap, /clear): read it again now.
    if [ -f "$HUB_LITE_RELOAD" ]; then
      rm -f "$HUB_LITE_RELOAD"
      VID=""
      # shellcheck disable=SC1090
      [ -f "$CONF" ] && . "$CONF"
      if [ -z "$VID" ]; then log "vehicle removed from this router - stopping reporting"; exec "$0"; fi
      load_config
      log "configuration reloaded (gps every ${GPS_INTERVAL}s, modem every ${MODEM_INTERVAL}s)"
      _next_gps=0; _next_modem=0
    fi
    if [ "$_now" -ge "$_next_gps" ]; then
      push_gps
      _next_gps=$(( $(date +%s) + GPS_INTERVAL ))
    fi
    if lt_configured; then
      LT_NAP_SLICE=5
      _woken=0
      [ -f "$LT_STATE_DIR/wake" ] && _woken=1
      if [ "$_woken" = "1" ] || [ "$(date +%s)" -ge "$_next_lt" ]; then
        # A command just landed at the gateway: let it apply before reading it back (the daemon's
        # LINKTAP_WAKE_SETTLE).
        [ "$_woken" = "1" ] && sleep 2
        linktap_tick
        _next_lt=$(( $(date +%s) + ${LINKTAP_POLL:-120} ))
        _hint=$(lt_poll_hint "$(date +%s)")
        [ -n "$_hint" ] && [ $(( $(date +%s) + _hint )) -lt "$_next_lt" ] && _next_lt=$(( $(date +%s) + _hint ))
        # Valve telemetry reaches the cloud promptly after a command, not on the next modem tick.
        [ "$_woken" = "1" ] && drain_relay
      fi
    else
      # No valve to be woken for, but a CGI's follow-up or reload flag should still land within half
      # a minute rather than a whole GPS interval (which a Cloud Update Schedule can make 30 min).
      LT_NAP_SLICE=30
    fi
    if [ "$(date +%s)" -ge "$_next_modem" ]; then
      push_modem
      # No-op once we have a key. Here rather than at startup so a box that boots with no WAN still
      # collects one the moment the uplink comes back. NOT gated on HUB_LITE_ENABLED: that flag is
      # the RELAY TIER, and the management door is not part of it.
      fetch_mgmt_key
      # 🔴 NOT GATED ON HUB_LITE_ENABLED ANY MORE. LinkTap telemetry, cycle ends, flood-close
      # records AND the plan gate + valve profiles all travel through this drain; gating it on the
      # relay tier meant a router with the tier off never reported a valve, never learned its
      # profiles, and grew its spool in RAM without bound. The drain is a no-op on an empty spool.
      drain_relay
      watch_hub
      _next_modem=$(( $(date +%s) + MODEM_INTERVAL ))
    elif relay_needs_retry; then
      # A failed batch or an undelivered alarm is retried on this loop's short cadence rather than
      # waiting out MODEM_INTERVAL (the daemon's shelly_retry_loop). Bounded by the loop's own
      # period, and a no-op while the spool is empty.
      if [ "$(date +%s)" -ge "${_next_retry:-0}" ]; then
        drain_relay
        _next_retry=$(( $(date +%s) + 30 ))
      fi
    fi
    spool_cap "$RELAY_SPOOL"
    if [ "$(date +%s)" -ge "$_next_update" ]; then
      update_check
      _next_update=$(( $(date +%s) + UPDATE_CHECK_SECS ))
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
      _next_gps=$(( $(date +%s) + GPS_INTERVAL ))
      _next_modem=$(( $(date +%s) + MODEM_INTERVAL ))
    fi
    _lt_due=""
    lt_configured && _lt_due=$_next_lt
    sleep "$(next_nap "$(date +%s)" "$_next_gps" "$_next_modem" "$_lt_due")"
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
