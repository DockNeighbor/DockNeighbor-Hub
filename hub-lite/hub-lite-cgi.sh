#!/bin/sh
# BRVG relay — CGI receiver (ONSITE.md "The one wire contract", relay tier). Installed at /www/brvg/cgi-bin/report and
# served by a dedicated uhttpd instance on the LAN. Pure POSIX shell: Lua is NOT stock firmware
# (owner correction 2026-08-13), and the X750's 416 KB of free overlay rules out everything else.
#
# The Shellys' webhooks are re-registered against this URL instead of the cloud:
#   http://<router-lan-ip>:8722/cgi-bin/report?device=<id>&event=<ev>&<values...>
# (8181 also answers, as RECEIVER_LEGACY_PORT, for sensors registered before the 2026-08-31 move.)
# so a sensor's report never leaves the LAN — which is what lets lockdown's forward chain be
# deny-all with no allow rules, and means the sensors need neither DNS nor a correct clock.
#
# TWO PATHS, decided by the event class:
#   * ALARMS (anything that is not *.measurement / *.change — the same line events.ts draws) are
#     sent to the cloud IMMEDIATELY as a single-item batch. Aggregation never delays an alarm.
#     Only if that send fails is the alarm spooled, so the next drain retries it — never both.
#   * telemetry is appended to the spool and rides the next roll-up.
#
# The spool line format is the relay's internal contract with brvg-hub-lite.sh:
#   <epoch>\t<device>\t<event>\t<raw-urlencoded-params>
# Appends of one short line to tmpfs are effectively atomic (< PIPE_BUF); there is no locking.

CONF="${BRVG_HUB_LITE_CONF:-/etc/brvg-hub-lite.conf}"
SPOOL="${BRVG_RELAY_SPOOL:-/tmp/brvg-relay.spool}"

# A sleepy flood sensor is awake on borrowed battery — answer first, work after.
printf 'Content-Type: text/plain\r\n\r\nok\r\n'

Q="${QUERY_STRING:-}"
device=""; event=""; rest=""; k=""; has_k=0
IFS='&'
for kv in $Q; do
  case "$kv" in
    device=*) device="${kv#device=}" ;;
    event=*)  event="${kv#event=}" ;;
    # The vehicle's webhook secret and vid are ROUTING, never telemetry. Before 0.15 a sensor URL
    # that carried `k=` had it spooled as a param and posted upstream in the batch body.
    k=*) k="${kv#k=}"; has_k=1 ;;
    vid=*) ;;
    '') ;;
    *) rest="${rest:+$rest&}$kv" ;;
  esac
done
unset IFS

# A PRESENTED secret must be the right one. This receiver's own contract (the relay tier, sensor
# URLs written without `k`) stays open to the LAN — refusing un-keyed reports here would silently
# cut every relay-routed sensor off from the local flood shutoff. But a report that DOES carry a
# `k` and gets it wrong is a misconfigured sensor or a guess, and is refused like the daemon refuses
# it. The deny-when-unset route with the daemon's full rule is /api/hub/shelly (hub-lite-api.sh).
if [ "$has_k" = "1" ] && [ -f "$CONF" ]; then
  _want=$(sed -n "s/^[[:space:]]*SHELLY_SECRET=[\"']\{0,1\}\([^\"']*\)[\"']\{0,1\}[[:space:]]*$/\1/p" "$CONF" 2>/dev/null | tail -1)
  # The secret alphabet (valid_secret) has no characters a sensor would percent-encode except + / =.
  k=$(printf '%s' "$k" | sed 's/%2[Bb]/+/g; s/%2[Ff]/\//g; s/%3[Dd]/=/g')
  if [ -n "$_want" ] && [ "$k" != "$_want" ]; then
    exit 0
  fi
fi

# Identity fields are constrained hard — they end up in storage keys and JSON. Values stay raw
# urlencoded here; the drain's parser owns decoding + escaping.
device=$(printf '%s' "$device" | tr -cd 'A-Za-z0-9_.:-' | cut -c1-64)
event=$(printf '%s' "$event" | tr -cd 'A-Za-z0-9_.-' | cut -c1-64)
[ -n "$device" ] && [ -n "$event" ] || exit 0

# Same telemetry line the worker draws (events.ts isTelemetry): measurements and changes batch;
# everything else — alarms, alarm-clears, button presses — goes NOW.
case "$event" in
  *.measurement|*.change) urgent=0 ;;
  *) urgent=1 ;;
esac

# A HARD CEILING, cheap enough to run on every webhook: the collector bounds the spool properly (oldest
# readings first, spool_cap) on every loop, but a sensor storm between two loops must not be able to
# fill RAM either. Past twice the collector's bound a READING is dropped here; an alarm never is.
SPOOL_CEILING=$(( ${BRVG_RELAY_SPOOL_MAX:-300} * 2 ))
spool_line() {
  if [ "$urgent" = "0" ] && [ -s "$SPOOL" ] && [ "$(wc -l < "$SPOOL" | tr -cd '0-9')" -ge "$SPOOL_CEILING" ]; then
    return 0
  fi
  printf '%s\t%s\t%s\t%s\n' "$(date +%s)" "$device" "$event" "$rest" >> "$SPOOL"
}

if [ "$urgent" = "1" ] && [ -f "$CONF" ]; then
  # shellcheck disable=SC1090
  . "$CONF"
  # LOCAL FLOOD -> VALVE SHUTOFF, before the cloud send (hub-lite capability #1, owner
  # 2026-08-19): the close must not wait on the WAN — with the LinkTap cloud gone this is the
  # only automated close when the uplink is down. Deliberately independent of DEVICE_TOKEN:
  # closing a valve on the LAN needs no cloud credential. One-shot sourcing of the hub-lite, same
  # pattern as spool_to_items below, so the classifier and the close live in ONE place.
  if [ -n "${LINKTAP_HOST:-}" ]; then
    LINKTAP_HOST="$LINKTAP_HOST" LINKTAP_GW_ID="${LINKTAP_GW_ID:-}" LINKTAP_DEV_IDS="${LINKTAP_DEV_IDS:-}" \
    BRVG_RELAY_SPOOL="$SPOOL" BRVG_HUB_LITE_TEST=1 \
      sh -c ". \"${BRVG_HUB_LITE_BIN:-/usr/bin/brvg-hub-lite}\"; is_flood_shutoff \"$event\" && linktap_flood_close" 2>/dev/null || true
  fi
  if [ -n "${DEVICE_TOKEN:-}" ] && [ -n "${VID:-}" ] && [ -n "${DEVICE_ID:-}" ]; then
    # Single-item batch, NO seq: this path never retries (failure falls through to the spool,
    # which has its own seq), so idempotency isn't needed and must not be claimed.
    # ONE decoder: reuse the hub-lite's own spool→items builder rather than carrying a copy of the
    # urldecode/escape awk here. Sourcing with BRVG_HUB_LITE_TEST=1 defines functions only — no loop.
    _items=$(printf '0\t%s\t%s\t%s\n' "$device" "$event" "$rest" \
      | BRVG_HUB_LITE_TEST=1 sh -c ". \"${BRVG_HUB_LITE_BIN:-/usr/bin/brvg-hub-lite}\"; spool_to_items" 2>/dev/null)
    case "$_items" in "["*"]") : ;; *) _items="" ;; esac
    _body='{"v":1,"kind":"delta","items":'${_items:-[]}',"ok":[]}'
    if [ -n "$_items" ] && curl -fsS --max-time 10 -X POST \
        -H 'Content-Type: application/json' -d "$_body" \
        "${WORKER_URL:-https://api.dockneighbor.com}/api/agent/batch?vid=${VID}&device=${DEVICE_ID}&t=${DEVICE_TOKEN}" \
        >/dev/null 2>&1; then
      exit 0   # delivered — do NOT also spool, or the next drain double-reports the alarm
    fi
  fi
fi

spool_line
exit 0
