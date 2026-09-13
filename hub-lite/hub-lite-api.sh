#!/bin/sh
# BRVG hub-lite — the /api/hub/* door. Installed at /www/brvg/api/hub and served by the same uhttpd
# instance as the webhook receiver, on port 8722.
#
# ONE CONTRACT (owner ruling 2026-08-31): "the hub-lite should move to 8722, keep one contract."
# The app used to speak two dialects — /api/hub/<verb> to the Rust daemon on 8722, and
# ?action=<verb> to a hub-lite on 8181. Same questions, two shapes, two sets of bugs. This file is
# the hub-lite answering the DAEMON's contract (daemon/src/hub_server.rs), so the app has one hub
# client and one mental model.
#
# uhttpd is started with `-x /api`, and resolves the longest existing file path before handing the
# remainder over as PATH_INFO. So this ONE script at /www/brvg/api/hub receives every verb beneath
# it: /api/hub/ping → PATH_INFO=/ping.
#
# AUTH, route by route, the daemon's split with one difference that is not a choice:
#   * OPEN — ping (liveness + version), identity/bootstrap (first run only), shelly (its own
#     secret), linktap/push (the gateway, by peer address). The same routes the daemon leaves open.
#   * THE MANAGEMENT KEY for everything else, as `Authorization: Bearer`. The daemon checks a
#     per-member `x-brvg-key` and resolves a role; a hub-lite has ONE key per router and one
#     privilege level (hubLiteKey.ts), and uhttpd hands a CGI no custom X-* headers at all
#     (bench GL-X750, 2026-08-21 — see hub-lite-mgmt.sh). Same secret, same header as the mgmt door.
#
# ⚠️ THE KEY COMPARISON IS A PLAIN STRING TEST, like the mgmt door's. POSIX sh has no constant-time
# compare; the daemon's ct_eq has no shell equivalent that is not itself leakier than `=`.
#
# ⚠️ EVERY Status LINE CARRIES ITS REASON PHRASE. uhttpd ignores a bare `Status: 401` and sends the
# body as 200 OK (bench-verified, hub-lite-mgmt.sh) — so the first cut of this file answered every
# refusal as a success carrying an error body. `reason` below is the only way a code goes out.

CONF="${BRVG_HUB_LITE_CONF:-/etc/brvg-hub-lite.conf}"
BIN="${BRVG_HUB_LITE_BIN:-/usr/bin/brvg-hub-lite}"
REPORT_CGI="${BRVG_REPORT_CGI:-/www/brvg/cgi-bin/report}"
LT_STATE_DIR="${BRVG_LT_STATE_DIR:-${LT_STATE_DIR:-/tmp/brvg-linktap}}"
STARTED_FILE="${BRVG_HUB_LITE_STARTED:-/tmp/brvg-hub-lite.started}"
UPDATE_FILE="${BRVG_HUB_LITE_UPDATE:-/tmp/brvg-hub-lite.update}"
RELOAD_FILE="${BRVG_HUB_LITE_RELOAD:-/tmp/brvg-hub-lite.reload}"
export LT_STATE_DIR

# The first-run window, from service start: the daemon's adopt::ADOPTION_WINDOW.
ADOPTION_WINDOW_SECS=900

# Request bodies are small JSON; anything bigger is not a request this door serves.
MAX_BODY=16384

reason() {
  case "$1" in
    200) echo '200 OK' ;;
    204) echo '204 No Content' ;;
    400) echo '400 Bad Request' ;;
    401) echo '401 Unauthorized' ;;
    402) echo '402 Payment Required' ;;
    403) echo '403 Forbidden' ;;
    404) echo '404 Not Found' ;;
    409) echo '409 Conflict' ;;
    422) echo '422 Unprocessable Entity' ;;
    500) echo '500 Internal Server Error' ;;
    501) echo '501 Not Implemented' ;;
    502) echo '502 Bad Gateway' ;;
    503) echo '503 Service Unavailable' ;;
    *)   echo "$1 Error" ;;
  esac
}

reply() { printf 'Status: %s\r\nContent-Type: application/json\r\n\r\n%s\r\n' "$(reason "$1")" "$2"; exit 0; }
fail() { reply "$1" "{\"error\":\"$(esc "$2")\"}"; }

# JSON string escaping, such as shell can manage. Control characters are dropped, not escaped:
# nothing this door emits legitimately carries one.
esc() { printf '%s' "$1" | tr -d '\000-\010\013\014\016-\037' | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk '{ printf "%s%s", (NR > 1 ? "\\n" : ""), $0 }'; }

# 🔴 A DAMAGED CONF IS NOT SOURCED. `.` on a file with a syntax error EXITS the shell under dash and
# busybox ash, so this door would die mid-request with no status line at all — the one moment the
# router most needs to explain itself (the daemon's configDamaged exists for the same moment).
CONF_DAMAGED=""
if [ -e "$CONF" ]; then
  if [ ! -r "$CONF" ]; then
    CONF_DAMAGED="the configuration file cannot be read"
  elif ! sh -n "$CONF" 2>/dev/null; then
    CONF_DAMAGED="the configuration file is not valid shell"
  else
    # shellcheck disable=SC1090
    . "$CONF"
  fi
fi

# Source the hub-lite for its functions ONLY — BRVG_HUB_LITE_TEST stops main() from running. One
# implementation of the valve open, the conf rewrite and the flood rule, whichever door calls it.
load_lib() {
  # shellcheck disable=SC1090
  BRVG_HUB_LITE_TEST=1 . "$BIN" 2>/dev/null || fail 500 "hub-lite not installed"
  CONF="${BRVG_HUB_LITE_CONF:-/etc/brvg-hub-lite.conf}"
}

# THE ONE AUTH DECISION. Every keyed route names the daemon role it needs (hub_server.rs may_*):
#   monitor    — read state (status, linktap/state)
#   control    — act on a device (linktap/valve) and read the log (the daemon's may_control)
#   configure  — change settings (config, gps)                     (may_configure)
#   administer — replace the credential or the software (token, clear, update) (may_administer)
#
# TODAY every role is satisfied by the router's single MGMT_KEY, because that is the only key a
# hub-lite holds. Owner decision D3 (2026-09-13) is per-member role keys like the daemon's
# member_keys, so control-access crew can open valves while monitor cannot; that change belongs
# HERE and nowhere else — no route checks a key itself. Keep it that way.
authorize() {
  [ -n "${MGMT_KEY:-}" ] || fail 503 "this hub-lite has no management key yet"
  case "$1" in
    monitor|control|configure|administer) : ;;
    *) fail 500 "unknown access level" ;;
  esac
  [ "${HTTP_AUTHORIZATION:-}" = "Bearer $MGMT_KEY" ] || fail 401 "a management key is required"
}

read_body() {
  _len=$(printf '%s' "${CONTENT_LENGTH:-0}" | tr -cd '0-9')
  _len=${_len:-0}
  [ "$_len" -gt "$MAX_BODY" ] && fail 400 "request body too large"
  [ "$_len" -gt 0 ] || { BODY=""; return 0; }
  BODY=$(head -c "$_len" | tr '\n\r' '  ')
}

# Flat-JSON field readers. Every field this door accepts is a short string, a number or a boolean,
# and each is validated before use; nothing here is a general parser, deliberately.
json_str()  { printf '%s' "$2" | grep -oE "\"$1\"[[:space:]]*:[[:space:]]*\"[^\"\\\\]*\"" | head -1 | sed 's/^[^:]*:[[:space:]]*"//; s/"$//'; }
json_num()  { printf '%s' "$2" | grep -oE "\"$1\"[[:space:]]*:[[:space:]]*-?[0-9][0-9.]*" | head -1 | sed 's/^[^:]*:[[:space:]]*//'; }
json_bool() { printf '%s' "$2" | grep -oE "\"$1\"[[:space:]]*:[[:space:]]*(true|false)" | head -1 | sed 's/^[^:]*:[[:space:]]*//'; }
json_has()  { printf '%s' "$2" | grep -qE "\"$1\"[[:space:]]*:"; }
# A string field that is present but not a plain string (an escaped quote, a number) is a 422, never
# a silent "absent": that is how a caller's value would otherwise be quietly ignored.
json_str_strict() {
  _v=$(json_str "$1" "$2")
  if [ -z "$_v" ] && json_has "$1" "$2" && ! printf '%s' "$2" | grep -qE "\"$1\"[[:space:]]*:[[:space:]]*\"\""; then
    fail 422 "$1 must be a plain string"
  fi
  printf '%s' "$_v"
}

hub_version() { sed -n 's/^HUB_LITE_VERSION="\([^"]*\)".*/\1/p' "$BIN" 2>/dev/null | head -1 | tr -cd '0-9A-Za-z.-'; }

now() { date +%s; }

service_uptime() {
  _st=$(tr -cd '0-9' < "$STARTED_FILE" 2>/dev/null)
  if [ -n "$_st" ]; then _u=$(( $(now) - _st )); [ "$_u" -lt 0 ] && _u=0; echo "$_u"; return; fi
  cut -d. -f1 /proc/uptime 2>/dev/null || echo 0
}

registered() { [ -n "${VID:-}" ] && [ -n "${DEVICE_TOKEN:-}${VEHICLE_KEY:-}" ]; }

# What this router can actually DO, as a JSON array. A capability is a promise the app acts on, so
# each is claimed only where the feature exists on THIS box right now.
#   * `linktap` exactly as the daemon's capabilities_of: the cloud's permission AND a gateway.
#   * The hub-lite-only vocabulary (owner decision D5, 2026-09-13): lockdown and local_admin need
#     uci (and dropbear for the latter); modem_at needs the modem's AT port to exist — a GL-MT300N-V2
#     has no modem and must not claim it; wan_usage needs /sys/class/net; usb_gps needs opkg, which
#     brvg-setup-usb-gps installs the serial drivers with; anchor_local is the collector itself.
#   * NOT claimed: routers, sensors, web_ui_toggle and gps_discover. They are daemon features (or D5
#     names) a hub-lite does not implement yet.
capabilities() {
  _c=""
  add_cap() { _c="${_c:+$_c,}\"$1\""; }
  if [ "${LINKTAP_ALLOWED:-0}" = "1" ] && [ -n "${LINKTAP_HOST:-}" ] && [ -n "${LINKTAP_GW_ID:-}" ]; then
    add_cap linktap
  fi
  _uci=0; command -v uci >/dev/null 2>&1 && _uci=1
  [ "$_uci" = "1" ] && add_cap lockdown
  [ -c "${AT_PORT:-/dev/ttyUSB2}" ] && add_cap modem_at
  add_cap anchor_local
  [ -d "${BRVG_SYS_NET:-/sys/class/net}" ] && add_cap wan_usage
  command -v opkg >/dev/null 2>&1 && add_cap usb_gps
  [ "$_uci" = "1" ] && [ -x /etc/init.d/dropbear ] && add_cap local_admin
  printf '[%s]' "$_c"
}

gps_json() {
  case "${GPS_SOURCE:-auto}" in
    cradlepoint)
      [ -n "${CRADLEPOINT_HOST:-}" ] || return 0
      _hp=false; [ -n "${CRADLEPOINT_PASSWORD:-}" ] && _hp=true
      printf ',"gps":{"kind":"cradlepoint","host":"%s","port":%s,"protocol":"tcp","username":"%s","devId":"","enabled":true,"hasPassword":%s}' \
        "$(esc "$CRADLEPOINT_HOST")" "$(printf '%s' "${CRADLEPOINT_PORT:-443}" | tr -cd '0-9')" "$(esc "${CRADLEPOINT_USER:-admin}")" "$_hp" ;;
    tcp)
      [ -n "${GPS_HOST:-}" ] || return 0
      printf ',"gps":{"kind":"nmea","host":"%s","port":%s,"protocol":"tcp","username":"","devId":"","enabled":true,"hasPassword":false}' \
        "$(esc "$GPS_HOST")" "$(printf '%s' "${GPS_PORT:-10110}" | tr -cd '0-9')" ;;
  esac
}

# The daemon's StatusBody, field for field, with honest values for a router. `lite: true` is the
# part the daemon does not send: a client must never assume a hub-lite is a full hub.
status_json() {
  _reg=false; registered && _reg=true
  _armed=false; [ -n "${SHELLY_SECRET:-}" ] && _armed=true
  _keys=0; [ -n "${MGMT_KEY:-}" ] && _keys=1
  _plat=linux; [ -f /etc/openwrt_release ] && _plat=openwrt
  _extra=""
  [ -n "$CONF_DAMAGED" ] && _extra="$_extra,\"configDamaged\":\"$(esc "$CONF_DAMAGED")\""
  _upd=$(tr -cd '0-9.' < "$UPDATE_FILE" 2>/dev/null)
  [ -n "$_upd" ] && _extra="$_extra,\"updateAvailable\":\"$_upd\""
  _hb=$(printf '%s' "${MODEM_INTERVAL:-600}" | tr -cd '0-9')
  _port=$(printf '%s' "${RECEIVER_PORT:-8722}" | tr -cd '0-9')
  printf '{"lite":true,"hubId":"%s","vid":"%s","name":"%s","enabled":true,"heartbeatSecs":%s,"httpPort":%s,"registered":%s,"version":"%s","platform":"%s","uptimeSecs":%s,"keysSynced":%s,"capabilities":%s,"shellyIngestArmed":%s%s%s,"webUiEnabled":false,"routers":[],"sensors":[]}' \
    "$(esc "${DEVICE_ID:-}")" "$(esc "${VID:-}")" "$(esc "${HUB_NAME:-}")" "${_hb:-600}" "${_port:-8722}" "$_reg" \
    "$(esc "$(hub_version)")" "$_plat" "$(service_uptime)" "$_keys" "$(capabilities)" "$_armed" "$_extra" "$(gps_json)"
}

# Re-read after a write, so the answer describes the conf as it now is.
reload_conf() {
  VID=""; DEVICE_ID=""; DEVICE_TOKEN=""; VEHICLE_KEY=""; MGMT_KEY=""; SHELLY_SECRET=""; HUB_NAME=""
  # shellcheck disable=SC1090
  [ -f "$CONF" ] && . "$CONF"
  : > "$RELOAD_FILE" 2>/dev/null
}

# --- validation (PURE) --------------------------------------------------------------------------
valid_port()  { case "$1" in ''|*[!0-9]*) return 1 ;; esac; [ "$1" -ge 1 ] && [ "$1" -le 65535 ]; }
valid_host()  { case "$1" in ''|*[!A-Za-z0-9.:-]*) return 1 ;; esac; [ "${#1}" -le 253 ]; }
valid_token() { case "$1" in *[!A-Za-z0-9_-]*) return 1 ;; esac; [ "${#1}" -ge 16 ] && [ "${#1}" -le 256 ]; }
valid_ident() { case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac; [ "${#1}" -le 64 ]; }
valid_secret() { case "$1" in *[!A-Za-z0-9._~+/=-]*) return 1 ;; esac; [ "${#1}" -le 128 ]; }
# A display name or a password: printable, no quote or backslash (the flat reader above cannot carry
# them), bounded. Everything else is safe, because conf_line quotes it before it reaches the conf.
valid_text() { case "$1" in *[\"\\]*) return 1 ;; esac; [ "${#1}" -le "${2:-64}" ] && [ -z "$(printf '%s' "$1" | tr -d '[:print:]')" ]; }

# Apply a daemon-shaped GPS body ({kind, host, port?, username?, password?, protocol?, enabled?}) to
# the conf-set argument list in GPS_ARGS. `devId` is accepted and ignored: a hub-lite reports GPS as
# itself, never as a separate gps device (the cloud lets only hub_ tokens vouch for one).
gps_args() {
  _g="$1"
  GPS_ARGS=""
  _kind=$(json_str_strict kind "$_g")
  _en=$(json_bool enabled "$_g")
  if [ "$_en" = "false" ]; then GPS_ARGS="GPS_SOURCE auto"; return 0; fi
  _host=$(json_str_strict host "$_g")
  _gport=$(json_num port "$_g")
  _user=$(json_str_strict username "$_g")
  _pass=$(json_str_strict password "$_g")
  _proto=$(json_str_strict protocol "$_g")
  valid_host "$_host" || fail 422 "host must be an address or a host name"
  [ -z "$_gport" ] || valid_port "$_gport" || fail 422 "port must be 1-65535"
  case "$_kind" in
    cradlepoint)
      [ -z "$_user" ] || valid_text "$_user" 64 || fail 422 "username is not valid"
      [ -z "$_pass" ] || valid_text "$_pass" 128 || fail 422 "password is not valid"
      GPS_ARGS="GPS_SOURCE cradlepoint CRADLEPOINT_HOST $_host CRADLEPOINT_PORT ${_gport:-443} CRADLEPOINT_USER ${_user:-admin}"
      [ -n "$_pass" ] && GPS_PASS="$_pass"
      ;;
    nmea)
      # UDP needs a listening nc, which stock GL.iNet busybox is not known to have (parity matrix §3.2).
      [ "${_proto:-tcp}" = "tcp" ] || fail 422 "a hub-lite reads NMEA over TCP only"
      GPS_ARGS="GPS_SOURCE tcp GPS_HOST $_host GPS_PORT ${_gport:-10110}"
      ;;
    *) fail 422 "kind must be cradlepoint or nmea" ;;
  esac
}

# The daemon's first_run_only: permanent state first (damaged, already claimed), then the caller
# (loopback always; the router's own /24 only inside the window from service start).
first_run_only() {
  [ -n "$CONF_DAMAGED" ] && fail 409 "this hub's configuration is damaged and must be repaired or removed first: $CONF_DAMAGED"
  [ -n "${VID:-}" ] || [ -n "${DEVICE_TOKEN:-}" ] && fail 409 "this hub is already set up; rotate its credential instead"
  _peer="${REMOTE_ADDR:-}"
  _peer="${_peer#::ffff:}"
  case "$_peer" in 127.*) return 0 ;; esac
  _pfx="${_peer%.*}"
  _same=0
  for _ip in $(ip -4 addr show 2>/dev/null | sed -n 's/.*inet \([0-9.]*\)\/.*/\1/p'); do
    [ "${_ip%.*}" = "$_pfx" ] && _same=1
  done
  [ "$_same" = "1" ] || fail 403 "this hub can only be set up from a device on the same local network"
  [ "$(service_uptime)" -le "$ADOPTION_WINDOW_SECS" ] || \
    fail 403 "this hub's setup window has closed — restart the hub service to open it again, then set the hub up within 15 minutes"
  logger -t brvg-hub-lite "setup: LAN setup call from $_peer accepted (claim window open)" 2>/dev/null || true
}

# PURE: urldecode one query value (+ and %XX). Used only on the Shelly secret and vid.
urldecode() {
  printf '%s' "$1" | awk '{
    s = $0; gsub(/\+/, " ", s); out = ""; hex = "0123456789abcdef"
    for (i = 1; i <= length(s); i++) {
      c = substr(s, i, 1)
      if (c == "%" && i + 2 <= length(s)) {
        h = tolower(substr(s, i + 1, 2))
        if (h ~ /^[0-9a-f][0-9a-f]$/) { out = out sprintf("%c", (index(hex, substr(h, 1, 1)) - 1) * 16 + index(hex, substr(h, 2, 1)) - 1); i += 2; continue }
      }
      out = out c
    }
    printf "%s", out
  }'
}

# The daemon's shelly_peer_plausible: RFC1918, CGNAT 100.64/10, loopback, link-local. Not
# authentication — the blast radius, checked before the secret so it discloses nothing.
peer_plausible() {
  _pp="${1#::ffff:}"
  case "$_pp" in
    10.*|192.168.*|127.*|169.254.*) return 0 ;;
    172.1[6-9].*|172.2[0-9].*|172.3[01].*) return 0 ;;
    100.*) _o2=$(printf '%s' "$_pp" | cut -d. -f2); [ "${_o2:-0}" -ge 64 ] && [ "${_o2:-0}" -le 127 ] && return 0 ;;
  esac
  return 1
}

verb="${PATH_INFO:-/ping}"
method="${REQUEST_METHOD:-GET}"

case "$method:$verb" in
  # ---- ping (OPEN) ------------------------------------------------------------------------------
  # How the app discovers what this box is before it holds any key. `lite: true` is the honest
  # part, and the app's LAN hub sweep must read it first: a hub-lite is never a full hub to adopt.
  GET:/ping)
    _reg=false; registered && _reg=true
    _dmg=false; [ -n "$CONF_DAMAGED" ] && _dmg=true
    _adopt=false
    if [ "$_reg" = "false" ] && [ -z "$CONF_DAMAGED" ] && [ -z "${VID:-}" ] && [ "$(service_uptime)" -le "$ADOPTION_WINDOW_SECS" ]; then
      _adopt=true
    fi
    _ver=$(hub_version)
    reply 200 "{\"ok\":true,\"lite\":true,\"version\":\"$(esc "${_ver:-0}")\",\"registered\":$_reg,\"adoptable\":$_adopt,\"configDamaged\":$_dmg}"
    ;;

  # ---- status -----------------------------------------------------------------------------------
  GET:/status)
    authorize monitor
    reply 200 "$(status_json)"
    ;;

  # ---- logs -------------------------------------------------------------------------------------
  # The collector logs to logread (init.d `stderr 1`). The last 300 of its lines, with every
  # credential this router holds REDACTED as literal strings, plus any `t=` / `k=` query value —
  # the daemon's rule is that a log never carries a secret, and a shell log line is one careless
  # `log "$_url"` away from breaking it.
  GET:/logs)
    authorize control
    command -v logread >/dev/null 2>&1 || reply 200 '{"path":"logread","lines":""}'
    _lines=$(logread 2>/dev/null | grep 'brvg-hub-lite' | tail -n 300 \
      | awk -v s1="${MGMT_KEY:-}" -v s2="${DEVICE_TOKEN:-}" -v s3="${SHELLY_SECRET:-}" -v s4="${VEHICLE_KEY:-}" -v s5="${CRADLEPOINT_PASSWORD:-}" '
          function scrub(line, sec,  i, out) {
            if (sec == "") return line
            out = ""
            while ((i = index(line, sec)) > 0) { out = out substr(line, 1, i - 1) "[redacted]"; line = substr(line, i + length(sec)) }
            return out line
          }
          {
            l = scrub(scrub(scrub(scrub(scrub($0, s1), s2), s3), s4), s5)
            while (match(l, /[?&](t|k)=[^&[:space:]"]+/)) {
              l = substr(l, 1, RSTART + 2) "[redacted]" substr(l, RSTART + RLENGTH)
              if (++guard > 50) break
            }
            print l
          }')
    reply 200 "{\"path\":\"logread\",\"lines\":\"$(esc "$_lines")\"}"
    ;;

  # ---- config -----------------------------------------------------------------------------------
  # name, heartbeatSecs (= MODEM_INTERVAL), shellySecret, and a daemon-shaped `gps` object. The
  # daemon's `enabled` and `webUiEnabled` are REFUSED rather than ignored: a hub-lite has no local
  # web app, and "disabled" would stop the service that serves this very door, leaving no LAN way
  # back. A caller told 422 knows; a caller told 200 would believe it.
  POST:/config)
    authorize configure
    read_body
    load_lib
    json_has enabled "$BODY" && fail 422 "enabled is not supported on a hub-lite"
    json_has webUiEnabled "$BODY" && fail 422 "a hub-lite has no local web app to switch"
    set --
    if json_has name "$BODY"; then
      _n=$(json_str_strict name "$BODY" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
      [ -n "$_n" ] || fail 422 "name must not be empty"
      valid_text "$_n" 64 || fail 422 "name must be printable text of at most 64 characters"
      set -- "$@" HUB_NAME "$_n"
    fi
    if json_has heartbeatSecs "$BODY"; then
      _h=$(json_num heartbeatSecs "$BODY" | tr -cd '0-9')
      [ -n "$_h" ] || fail 422 "heartbeatSecs must be a number"
      [ "$_h" -ge 60 ] || fail 422 "heartbeatSecs must be at least 60"
      set -- "$@" MODEM_INTERVAL "$_h"
    fi
    if json_has shellySecret "$BODY"; then
      # Trimmed, because it arrives from a copy/paste field and a trailing newline would silently
      # break every comparison against it. An EMPTY string disarms, deliberately.
      _sec=$(json_str_strict shellySecret "$BODY" | tr -d '[:space:]')
      valid_secret "$_sec" || fail 422 "shellySecret is not a valid webhook secret"
      set -- "$@" SHELLY_SECRET "$_sec"
    fi
    if json_has gps "$BODY"; then
      _gobj=$(printf '%s' "$BODY" | sed -n 's/.*"gps"[[:space:]]*:[[:space:]]*{\([^{}]*\)}.*/\1/p')
      [ -n "$_gobj" ] || fail 422 "gps must be an object"
      GPS_PASS=""
      gps_args "{$_gobj}"
      # shellcheck disable=SC2086
      set -- "$@" $GPS_ARGS
      [ -n "$GPS_PASS" ] && set -- "$@" CRADLEPOINT_PASSWORD "$GPS_PASS"
    fi
    [ $# -gt 0 ] || fail 422 "nothing to change"
    conf_set "$@" || fail 500 "this router cannot write its own configuration"
    reload_conf
    reply 200 "$(status_json)"
    ;;

  # ---- gps (the daemon's own route, same body as config.gps) ------------------------------------
  POST:/gps)
    authorize configure
    read_body
    load_lib
    GPS_PASS=""
    gps_args "$BODY"
    # shellcheck disable=SC2086
    set -- $GPS_ARGS
    [ -n "$GPS_PASS" ] && set -- "$@" CRADLEPOINT_PASSWORD "$GPS_PASS"
    conf_set "$@" || fail 500 "this router cannot write its own configuration"
    reload_conf
    reply 200 "$(status_json)"
    ;;

  # ---- token ------------------------------------------------------------------------------------
  # The rotated device token after the app re-enrolls. Validated to the token alphabet, because it
  # lands in a sourced conf and in every report URL.
  POST:/token)
    authorize administer
    read_body
    load_lib
    _t=$(json_str_strict token "$BODY")
    [ -n "$_t" ] || fail 422 "token must not be empty"
    valid_token "$_t" || fail 422 "token is not a device token"
    conf_set DEVICE_TOKEN "$_t" || fail 500 "this router cannot write its own configuration"
    reload_conf
    reply 200 "$(status_json)"
    ;;

  # ---- clear ------------------------------------------------------------------------------------
  # The local half of un-registering, as the daemon's do_clear: forget the vehicle and every
  # credential. The CALLER revokes the enrollment with the worker. The collector sees the reload,
  # finds no vehicle and waits for setup rather than crash-looping. DEVICE_ID is kept: it names the
  # router's record in the app, and /api/hub/identity hands it back on the next setup.
  POST:/clear)
    authorize administer
    load_lib
    conf_set VID "" DEVICE_TOKEN "" VEHICLE_KEY "" MGMT_KEY "" SHELLY_SECRET "" LINKTAP_ALLOWED 0 HUB_NAME "" \
      || fail 500 "this router cannot write its own configuration"
    : > "$RELOAD_FILE" 2>/dev/null
    printf 'Status: %s\r\n\r\n' "$(reason 204)"
    exit 0
    ;;

  # ---- update -----------------------------------------------------------------------------------
  # The existing argument-free self_update (signed feed, smoke check, rollback), started in the
  # background because it restarts the service that is serving this reply. The version moving is
  # the confirmation, exactly as on the daemon.
  POST:/update)
    authorize administer
    command -v opkg >/dev/null 2>&1 || fail 501 "remote update is not supported on this platform - reinstall from the app"
    [ -r "$BIN" ] || fail 500 "hub-lite not installed"
    # setsid where it exists, so the service restart the update performs does not take the updater
    # down with the uhttpd that spawned it.
    _detach=""; command -v setsid >/dev/null 2>&1 && _detach=setsid
    $_detach sh -c "BRVG_HUB_LITE_TEST=1 . \"$BIN\"; self_update" </dev/null >/dev/null 2>&1 &
    reply 200 '{"status":"update started"}'
    ;;

  # ---- identity / bootstrap (OPEN, first run only) ----------------------------------------------
  # Setup from the app with no SSH, the daemon's h_identity/h_bootstrap: open only while the router
  # is unclaimed, and only from loopback or the router's own /24 within 15 minutes of service start.
  # The id is a `brv_net_` device id, because that is what a router enrolls as (agentToken.ts); a
  # router whose id the app already minted keeps it.
  GET:/identity)
    first_run_only
    load_lib
    if [ -z "${DEVICE_ID:-}" ]; then
      _hex=$(od -An -N6 -tx1 /dev/urandom 2>/dev/null | tr -cd '0-9a-f')
      [ -n "$_hex" ] || fail 500 "this router cannot mint an id"
      DEVICE_ID="brv_net_$_hex"
      conf_set DEVICE_ID "$DEVICE_ID" || fail 500 "this hub cannot write its own configuration"
    fi
    reply 200 "{\"hubId\":\"$(esc "$DEVICE_ID")\"}"
    ;;

  POST:/bootstrap)
    first_run_only
    read_body
    load_lib
    _vid=$(json_str_strict vid "$BODY")
    _tok=$(json_str_strict token "$BODY")
    [ -n "$_vid" ] && [ -n "$_tok" ] || fail 422 "vid and token are required"
    valid_ident "$_vid" || fail 422 "vid is not a vehicle id"
    valid_token "$_tok" || fail 422 "token is not a device token"
    _dev=$(json_str_strict deviceId "$BODY")
    [ -n "$_dev" ] || _dev="${DEVICE_ID:-}"
    [ -n "$_dev" ] || fail 422 "ask /api/hub/identity first, or send deviceId"
    valid_ident "$_dev" || fail 422 "deviceId is not a device id"
    _nm=$(json_str_strict name "$BODY" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
    [ -n "$_nm" ] || _nm="Hub"
    valid_text "$_nm" 64 || fail 422 "name must be printable text of at most 64 characters"
    set -- VID "$_vid" DEVICE_ID "$_dev" DEVICE_TOKEN "$_tok" HUB_NAME "$_nm"
    if json_has heartbeatSecs "$BODY"; then
      _h=$(json_num heartbeatSecs "$BODY" | tr -cd '0-9')
      [ -n "$_h" ] && [ "$_h" -ge 60 ] || fail 422 "heartbeatSecs must be at least 60"
      set -- "$@" MODEM_INTERVAL "$_h"
    fi
    conf_set "$@" || fail 500 "this hub cannot write its own configuration"
    reload_conf
    reply 200 "$(status_json)"
    ;;

  # ---- shelly (OPEN + the vehicle's webhook secret) ---------------------------------------------
  # The daemon's h_shelly, route for route: plausible peer, then the secret — DENY WHEN UNSET, like
  # the daemon, because this route closes valves and injects alerts — then the vehicle. On success
  # the report is handed to the webhook receiver with `k` and `vid` STRIPPED, so the flood close and
  # the forward run through one implementation, and the secret is never spooled or sent upstream as
  # a telemetry field.
  GET:/shelly|POST:/shelly)
    _k=""; _svid=""; _fwd=""
    IFS='&'
    for _kv in ${QUERY_STRING:-}; do
      case "$_kv" in
        k=*) _k=$(urldecode "${_kv#k=}") ;;
        vid=*) _svid=$(urldecode "${_kv#vid=}") ;;
        '') ;;
        *) _fwd="${_fwd:+$_fwd&}$_kv" ;;
      esac
    done
    unset IFS
    if ! peer_plausible "${REMOTE_ADDR:-}"; then
      logger -t brvg-hub-lite "shelly: REFUSED a report from ${REMOTE_ADDR:-?} - not a plausible vessel network" 2>/dev/null || true
      fail 403 "not a local caller"
    fi
    if [ -z "${SHELLY_SECRET:-}" ]; then
      logger -t brvg-hub-lite "shelly: REFUSED a report - this router holds no webhook secret; set it in the app or the local flood close CANNOT run" 2>/dev/null || true
      fail 401 "this hub has no webhook secret configured"
    fi
    [ "$_k" = "$SHELLY_SECRET" ] || fail 401 "missing or wrong webhook secret"
    [ -z "$_svid" ] || [ -z "${VID:-}" ] || [ "$_svid" = "$VID" ] || fail 404 "that vehicle is not this hub's"
    # Answer FIRST: a sleepy sensor is awake on borrowed battery. The receiver does the close and the
    # forward after, with its own reply discarded.
    printf 'Status: %s\r\nContent-Type: application/json\r\n\r\n{"ok":true}\r\n' "$(reason 200)"
    [ -r "$REPORT_CGI" ] && QUERY_STRING="$_fwd" sh "$REPORT_CGI" >/dev/null 2>&1
    exit 0
    ;;

  # ---- linktap/state ----------------------------------------------------------------------------
  # The last measurement the poll loop produced per valve, as the SAME params the cloud receives —
  # the daemon's do_valve_state, keyed like it (the daemon requires a member key; this requires the
  # router's). A valve with no measurement yet is OMITTED, not invented as closed: the daemon's
  # rule, and the app reads absence as "no reading".
  #
  # NO LONG POLL, deliberately: `wait` is accepted and ignored. uhttpd runs at most 3 CGIs at once
  # by default, and holding one for a minute per app would starve the webhook receiver — the flood
  # report's own door. Slower is allowed; a flood report refused for a busy worker is not.
  GET:/linktap/state)
    authorize monitor
    _valves=""
    for _raw in $(printf '%s' "${LINKTAP_DEV_IDS:-}" | tr ',' ' '); do
      _d=$(printf '%s' "$_raw" | tr -cd 'A-Za-z0-9' | cut -c1-16)
      [ -n "$_d" ] && [ -r "$LT_STATE_DIR/meas.$_d" ] || continue
      _pairs=$(tr -d '\n' < "$LT_STATE_DIR/meas.$_d" | tr '&' '\n' | awk -F= '
        $1 ~ /^[A-Za-z0-9_]+$/ && NF >= 2 { v = substr($0, length($1) + 2); gsub(/["\\]/, "", v); printf ",\"%s\":\"%s\"", $1, v }')
      _valves="${_valves:+$_valves,}{\"devId\":\"$_d\"$_pairs}"
    done
    _rev=$(tr -cd '0-9' < "$LT_STATE_DIR/rev" 2>/dev/null)
    reply 200 "{\"rev\":${_rev:-0},\"lite\":true,\"valves\":[${_valves}]}"
    ;;

  # ---- linktap/valve ----------------------------------------------------------------------------
  # The daemon's do_valve: {devId, action, durationSecs?, volumeCapL?, mode?, resumeNormal?}.
  #
  # PLAN GATE ON OPEN ONLY. An open is the paid feature and spends water, so it needs the cloud's
  # permission (402 otherwise) exactly as the daemon's. A CLOSE is never refused for the plan here:
  # it spends no water and removes no limit, the same reasoning the daemon's own flood close states,
  # and a lapsed plan must not be the reason a running valve cannot be shut from the app.
  POST:/linktap/valve)
    authorize control
    read_body
    load_lib
    _dev=$(json_str devId "$BODY" | tr -cd 'A-Za-z0-9' | cut -c1-16)
    _action=$(json_str action "$BODY")
    _mode=$(json_str mode "$BODY" | tr 'A-Z' 'a-z' | tr -d '[:space:]')
    _secs=$(json_num durationSecs "$BODY")
    _capl=$(json_num volumeCapL "$BODY")
    _resume=$(json_bool resumeNormal "$BODY")
    [ -n "${LINKTAP_HOST:-}" ] && [ -n "${LINKTAP_GW_ID:-}" ] || fail 409 "no LinkTap gateway is configured on this hub"
    [ -n "$_dev" ] || fail 422 "devId is required"
    lt_is_watched "$_dev" || fail 404 "that valve is not configured on this hub"
    # Unknown modes read as normal, exactly as cycle::Mode::from_wire.
    case "$_mode" in washdown|tankfill) : ;; *) _mode=normal ;; esac
    case "$_action" in
      close)
        # Marked only once the gateway TOOK the stop: a run carrying a stop is never volume-cut or
        # handed over, so a close that failed must not leave the running cycle looking stopped.
        lt_post "$(linktap_stop_body "$LINKTAP_GW_ID" "$_dev")" 10 >/dev/null \
          || fail 502 "the gateway did not accept that command"
        lt_mark_stop "$LT_STATE_DIR/$_dev" manual
        lt_wake
        reply 200 '{"ok":true}'
        ;;
      open)
        lt_open_allowed || fail 402 "this vehicle's plan does not include valve control"
        case "$_secs" in ''|*[!0-9]*) fail 422 "durationSecs is required to open a valve" ;; esac
        [ "$_secs" -gt 0 ] || fail 422 "durationSecs is required to open a valve"
        # ⚠️ A WASHDOWN IS TIME-ONLY (owner spec 2026-07-30, re-ratified twice). A volumeCapL sent
        # with mode=washdown is a caller bug; honouring it would re-create the external cap that cut
        # two-hour hose runs at ~26 gal. Refused, like the daemon.
        if [ "$_mode" = "washdown" ] && json_has volumeCapL "$BODY"; then
          fail 422 "a washdown is time-limited only - do not send volumeCapL with mode=washdown"
        fi
        if [ -n "$_capl" ] && ! awk -v c="$_capl" 'BEGIN{exit !(c > 0)}'; then
          fail 422 "volumeCapL must be a positive number of litres"
        fi
        lt_profile "$_dev"
        if [ "$_mode" = "washdown" ]; then
          _send=""; _track=0
        elif [ -n "$_capl" ]; then
          _send="$_capl"; _track="$_capl"
        else
          # 🔴 NO CAP SENT IS NOT NO CAP. The first cut tracked this as cap 0, which disables the
          # software cutoff — the only volume enforcement there is — for any Normal Run opened
          # without volumeCapL. The daemon tracks the valve's profile cap; so does this.
          _send=""; _track="$_p_cap"
        fi
        _res=0; [ "$_resume" = "true" ] && _res=1
        lt_open "$_dev" "$_secs" "$_send" "$_track" "$_mode" "$_res" \
          || fail 502 "the gateway did not accept that command"
        reply 200 '{"ok":true}'
        ;;
      *) fail 422 "unknown action - expected open or close" ;;
    esac
    ;;

  # ---- linktap/push (OPEN, the gateway) ---------------------------------------------------------
  # The gateway's HTTP push (full status on every change). INERT: it changes nothing but when the
  # poll loop next looks, which it rings for — the poll then reads the gateway itself, so a spoofed
  # push costs one extra cmd 3 and nothing else. Checked against the configured gateway address
  # anyway, the daemon's push_peer_allowed. Same "ok" whatever happens, so a prober learns nothing.
  POST:/linktap/push)
    _gh="${LINKTAP_HOST:-}"; _gh="${_gh%%:*}"
    _allow=0
    if [ -n "$_gh" ]; then
      case "$_gh" in
        *[!0-9.]*) _allow=1 ;;
        *) [ "${REMOTE_ADDR#::ffff:}" = "$_gh" ] && _allow=1 ;;
      esac
    fi
    if [ "$_allow" = "1" ]; then
      read_body
      load_lib
      for _pd in $(printf '%s' "$BODY" | lt_parse_push); do
        if lt_is_watched "$_pd"; then lt_wake; break; fi
      done
    fi
    printf 'Status: %s\r\nContent-Type: text/plain\r\n\r\nok\r\n' "$(reason 200)"
    exit 0
    ;;

  *)
    fail 404 "no such hub endpoint"
    ;;
esac
