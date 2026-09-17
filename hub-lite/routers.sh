#!/bin/sh
# BRVG hub-lite — managed routers (owner decision D2, 2026-09-13: "i plan on using GL.iNet
# GL-MT300N-V2 as the default hub lite for someone to buy if lets say they have a nice cradlepoint
# router with wifi aps throughout the yacht").
#
# A GL.iNet running hub-lite signs in to a SECOND router on the LAN — a Cradlepoint (NCOS) or a
# Peplink — reads its modem/WAN/GPS on a cadence, reports AS that router with the router's own
# agent token, and carries out the owner's writes (APN, GPS switch, reboot) on request. This file is
# a port of the DAEMON's managed routers (brvg-hub daemon/src/routers.rs + peplink.rs + the
# `/api/hub/routers` door in hub_server.rs): the snapshot, the `modem.measurement` param names and the
# door's JSON shapes are the daemon's, so the cloud and the app cannot tell which of the two polled.
#
# NOT Starlink (D2). A dish is the daemon's alone.
#
# Sourced by brvg-hub-lite.sh (collector + every CGI that loads it). Nothing here runs at source time.
#
# ⚠️ NO jsonfilter, deliberately. It IS in OpenWrt base (base-files DEPENDS += jsonfilter, 21.02
# through main), but it cannot sort a map the way the daemon's serde BTreeMap does, cannot re-emit a
# scrubbed tree for `read`, and does not exist on the Mac/Ubuntu hosts test.sh runs on — a parser
# the suite cannot run is a parser nobody checked. `rt_flat` below is one awk tokenizer instead.
#
# CREDENTIALS: router passwords live in $RT_CONF (root-only, mode 600), never in the world-readable
# conf, never in argv (curl reads its config, credential included, from stdin — `ps` on the router
# shows nothing), never in a log line, and never in any answer (only `hasPassword`).

RT_CONF="${BRVG_HUB_LITE_ROUTERS_CONF:-/etc/brvg-hub-lite.routers}"
RT_DIR="${BRVG_ROUTERS_STATE:-/tmp/brvg-routers}"
RT_FS=$(printf '\037')
RT_TAB=$(printf '\t')
RT_MAX_READ=262144
RT_CAPS_CP='["modem","wan","gps","gpsSwitch","apn","reboot","read","signin"]'
RT_CAPS_PL='["modem","wan","gps","signin"]'
RT_REFUSED="the router refused the sign-in — check the admin username and password"

rt_log() {
  if [ -n "${BRVG_RT_LOG:-}" ]; then echo "routers: $*" >> "$BRVG_RT_LOG"; return 0; fi
  logger -t brvg-hub-lite "routers: $*" 2>/dev/null || echo "brvg-hub-lite: routers: $*" >&2
}

# --- JSON (PURE) --------------------------------------------------------------------------------

# stdin JSON → one line per value: `path<TAB>type<TAB>value`, path segments joined by `/`, array
# indices numeric, type o|a|s|n|b|z. A parse error ends with the line `<TAB>!<TAB>`.
#
# `rt_flat J <path>` instead prints the value at <path> ("" = the root) back out as compact JSON,
# with every scalar under a secret-looking key replaced — the daemon's scrub_secrets, same needles,
# same rule that an OBJECT or ARRAY under such a key is descended into rather than blanked. Printed
# as it is tokenized, never accumulated: a config tree through a string buffer is quadratic on MIPS.
rt_flat() {
  awk -v J="${1:-}" -v RP="${2:-}" -v B="$(printf '\342\200\242\342\200\242\342\200\242')" '
  function ws(c) { while (i <= n) { c = substr(s, i, 1); if (c != " " && c != "\t" && c != "\n" && c != "\r") return; i++ } }
  function out(x) { if (cap) printf "%s", x }
  function jstr(c, o, st) {
    st = i; i++; o = ""
    while (i <= n) {
      c = substr(s, i, 1)
      if (c == "\"") { i++; S = o; R = substr(s, st, i - st); return 1 }
      if (c == "\\") { c = substr(s, ++i, 1); if (c == "u") { o = o "?"; i += 5; continue } o = o (c ~ /[nrtbf]/ ? " " : c); i++; continue }
      o = o c; i++
    }
    return 0
  }
  function val(p, k, m, r) { m = (J && p == RP); if (m) cap = 1; r = v2(p, k); if (m) cap = 0; return r }
  function v2(p, k, c, j, first, idx, key, sec) {
    ws(); if (i > n) return 0
    c = substr(s, i, 1)
    if (c == "{") {
      if (!J) print p "\to\t"
      out("{"); i++; ws()
      if (substr(s, i, 1) == "}") { i++; out("}"); return 1 }
      first = 1
      while (1) {
        ws(); if (substr(s, i, 1) != "\"" || !jstr()) return 0
        key = S; if (!first) out(","); first = 0; out(R ":")
        ws(); if (substr(s, i, 1) != ":") return 0
        i++
        if (!val(p == "" ? key : p "/" key, key)) return 0
        ws(); c = substr(s, i++, 1)
        if (c == "}") { out("}"); return 1 }
        if (c != ",") return 0
      }
    }
    if (c == "[") {
      if (!J) print p "\ta\t"
      out("["); i++; ws()
      if (substr(s, i, 1) == "]") { i++; out("]"); return 1 }
      idx = 0
      while (1) {
        if (idx) out(",")
        if (!val(p == "" ? idx : p "/" idx, "")) return 0
        idx++; ws(); c = substr(s, i++, 1)
        if (c == "]") { out("]"); return 1 }
        if (c != ",") return 0
      }
    }
    sec = (k != "" && tolower(k) ~ /password|passwd|secret|wpapsk|psk|private_key|community|token|shared_key/)
    if (c == "\"") {
      if (!jstr()) return 0
      gsub(/[\t\n\r]/, " ", S)
      if (!J) print p "\ts\t" S
      out(sec ? "\"" B "\"" : R); return 1
    }
    if (substr(s, i, 4) == "null") { if (!J) print p "\tz\t"; out("null"); i += 4; return 1 }
    j = (substr(s, i, 4) == "true") ? 4 : (substr(s, i, 5) == "false") ? 5 : 0
    if (j) { if (!J) print p "\tb\t" substr(s, i, j); out(sec ? "\"" B "\"" : substr(s, i, j)); i += j; return 1 }
    j = i
    while (i <= n && index("+-0123456789.eE", substr(s, i, 1))) i++
    if (i == j) return 0
    if (!J) print p "\tn\t" substr(s, j, i - j)
    out(sec ? "\"" B "\"" : substr(s, j, i - j)); return 1
  }
  { s = s $0 "\n" }
  END {
    n = length(s); i = 1
    if (!val("", "")) { if (!J) print "\t!\t"; exit 1 }
    ws(); if (i <= n) { if (!J) print "\t!\t"; exit 1 }
  }'
}

# The awk prelude every parser below reads flat lines with. T/V by path; K[parent] = child keys in
# document order. `kids(p, a, 1)` sorts them BYTEWISE — the daemon's serde_json Map is a BTreeMap,
# so "first entry" there means smallest key, and a mirror that took document order would pick a
# different SIM on a dual-modem unit.
RT_LIB='
BEGIN { CONVFMT = "%.10g"; OFMT = "%.10g" }
{ T[$1] = $2; V[$1] = $3
  if ($1 != "") { kk = $1; pp = ""; if (match(kk, /\/[^\/]*$/)) { pp = substr(kk, 1, RSTART - 1); kk = substr(kk, RSTART + 1) } K[pp] = K[pp] "\037" kk } }
function has(p) { return (p in T) }
function ty(p) { return (p in T) ? T[p] : "" }
function str(p, v) { if (ty(p) != "s") return ""; v = V[p]; sub(/^[ \t]+/, "", v); sub(/[ \t]+$/, "", v); return v }
function num(p, v) { if (ty(p) != "n" && ty(p) != "s") return ""; v = V[p]; sub(/^[ \t]+/, "", v); sub(/[ \t]+$/, "", v)
  return (v ~ /^[-+]?([0-9]+\.?[0-9]*|\.[0-9]+)([eE][-+]?[0-9]+)?$/) ? v + 0 : "" }
function nf(x) { return (x == int(x)) ? sprintf("%.0f", x) : sprintf("%.10g", x) }
function kids(p, a, srt, m, x, y, t) { m = split(substr(K[p], 2), a, "\037")
  if (srt) for (x = 2; x <= m; x++) for (y = x; y > 1 && (a[y] "") < (a[y - 1] ""); y--) { t = a[y]; a[y] = a[y - 1]; a[y - 1] = t }
  return m }
function o(k, a, b, c, v) { v = a; if (v == "") v = b; if (v == "") v = c; if (v != "") print k "\t" v }
function ser(p, a, m, x, r) {
  if (ty(p) == "o") { m = kids(p, a, 1); r = ""; for (x = 1; x <= m; x++) r = r (x > 1 ? "," : "") "\"" a[x] "\":" ser(p == "" ? a[x] : p "/" a[x]); return "{" r "}" }
  if (ty(p) == "a") { m = kids(p, a, 0); r = ""; for (x = 1; x <= m; x++) r = r (x > 1 ? "," : "") ser(p "/" a[x]); return "[" r "]" }
  if (ty(p) == "s") return "\"" V[p] "\""
  if (ty(p) == "z") return "null"
  return V[p] }
'

# stdin flat body → `!reason`, or the payload re-rooted under `D`. $1 ncos|peplink.
#   NCOS (routers.rs ncos_data): {success:false} → reason, or data whatever its shape (an object is
#   serialized — the CBA850 puts a failed write's per-field complaint there); data → payload.
#   Peplink (peplink.rs peplink_response): stat ok → response (or the body); fail → message.
rt_unwrap() {
  awk -F "$RT_TAB" -v V_="$1" "$RT_LIB"'
  function reroot(from, l, pth) {
    for (l = 1; l <= NL; l++) { pth = LP[l]
      if (from == "") print (pth == "" ? "D" : "D/" pth) "\t" LT[l] "\t" LV[l]
      else if (pth == from) print "D\t" LT[l] "\t" LV[l]
      else if (index(pth, from "/") == 1) print "D/" substr(pth, length(from) + 2) "\t" LT[l] "\t" LV[l] } }
  { NL++; LP[NL] = $1; LT[NL] = $2; LV[NL] = $3 }
  END {
    if (ty("") != "o") { print "!the router did not answer with JSON"; exit }
    if (V_ == "ncos") {
      if (ty("success") == "b" && V["success"] == "false") {
        w = has("reason") && ty("reason") != "z" ? "reason" : has("data") && ty("data") != "z" ? "data" : ""
        if (w != "" && ty(w) == "s" && str(w) != "") { print "!" str(w); exit }
        if (w != "" && ty(w) != "s") { d = ser(w); if (length(d) > 300) d = substr(d, 1, 300) "…"; print "!the router refused the request: " d; exit }
        print "!the router refused the request"; exit
      }
      if (has("data")) { reroot("data"); exit }
      if (ty("success") == "b" && V["success"] == "true") { reroot(""); exit }
      print "!unexpected response from the router"; exit
    }
    if (str("stat") == "ok") { reroot(has("response") ? "response" : ""); exit }
    if (str("stat") == "fail") { m = str("message"); print "!" (m != "" ? m : "the router refused the request"); exit }
    print "!unexpected response from the router"
  }'
}

# --- Cradlepoint NCOS parsers (PURE; stdin = payload flat under D) ------------------------------
# Fixtures = the 2026-08-17 CBA850 capture, the same ones routers.rs pins.

# /api/status/wan/devices → normalized m.* (the modem) and w.* (the connected WAN) lines.
rt_cp_status() {
  awk -F "$RT_TAB" "$RT_LIB"'
  function conn(e) { return tolower(str(e "/status/connection_state")) == "connected" }
  END {
    m = kids("D", U, 1); ent = 0; first = ""; pick = ""; wp = ""
    for (x = 1; x <= m; x++) {
      e = "D/" U[x]; if (ty(e) != "o") continue
      ent++; if (wp == "" && conn(e)) wp = e
      d = e "/diagnostics"
      if (ty(d) == "o" && (has(d "/CARRID") || has(d "/RSRP") || has(d "/HOMECARRID") || has(d "/MDN"))) {
        if (first == "") first = e
        if (pick == "" && conn(e)) pick = e
      }
    }
    if (pick == "") pick = first
    if (pick != "") {
      g = pick "/diagnostics"
      sm = str(g "/PIN_STATUS"); if (sm == "") sm = str(g "/SIM"); if (sm == "") sm = str(g "/SIM_STATUS")
      sm = tolower(sm)
      print "m.sim\t" ((sm == "ready" || sm == "ok") ? "ok" : (sm == "sim absent" || sm == "absent" || sm == "missing" || sm == "nosim") ? "missing" : (sm == "locked" || sm == "pin locked" || sm == "sim locked") ? "locked" : "unknown")
      o("m.carrier", str(g "/CARRID"), str(g "/HOMECARRID"))
      o("m.mode", str(g "/SERDIS"), str(g "/MODEMSYSMODE"), str(g "/SRVC_TYPE"))
      o("m.rssi", has(g "/DBM") ? num(g "/DBM") : num(g "/RSSI"))
      o("m.rsrp", num(g "/RSRP")); o("m.rsrq", num(g "/RSRQ")); o("m.sinr", num(g "/SINR"))
      if (conn(pick)) print "m.connected\t1"
      o("m.ip", str(pick "/status/ipinfo/ip_address"))
      t = num(pick "/stats/out"); if (t != "" && t >= 0) print "m.txBytes\t" nf(int(t))
      t = num(pick "/stats/in"); if (t != "" && t >= 0) print "m.rxBytes\t" nf(int(t))
    }
    if (ent) {
      if (wp == "") { print "w.wan\tnone"; print "w.up\t0" }
      else { k = tolower(substr(wp, 3)); print "w.wan\t" (k ~ /^mdm/ ? "lte" : k ~ /^wwan/ ? "repeater" : "wired"); print "w.up\t1"; o("w.ip", str(wp "/status/ipinfo/ip_address")) }
    }
  }'
}

# product_info payload under D, fw_info payload under F (either may be absent) → p.* lines, or none.
rt_cp_probe() {
  awk -F "$RT_TAB" "$RT_LIB"'
  END {
    md = str("D/product_name"); if (md == "") md = str("D")
    fw = ""
    if (ty("F") == "o" && num("F/major_version") != "") {
      split("major_version minor_version patch_version", ks, " ")
      for (x = 1; x <= 3; x++) { t = num("F/" ks[x]); if (t != "") fw = fw (fw == "" ? "" : ".") int(t) }
    } else fw = str("F")
    if (md == "" && fw == "") exit
    print "p._\t1"; o("p.model", md); o("p.firmware", fw); o("p.mac", str("D/mac0"))
  }'
}

# /api/config/wan/rules2 → "index<TAB>mode<TAB>apn". Among `mdm` rules one with a `modem` subtree
# wins and a uid-specific one beats a class rule — the #135 fix: MVP's CBA850 kept its manual
# `mw01.VZWSTATIC` in rule 4, and the first cut read the generic rule 1 as AUTOMATIC. NCOS spells
# automatic `default`; `auto` is the app's word.
rt_cp_apn() {
  awk -F "$RT_TAB" "$RT_LIB"'
  END {
    if (ty("D") != "a") exit
    m = kids("D", U, 0); best = -1; bi = ""
    for (x = 1; x <= m; x++) {
      r = "D/" U[x]; tr = (ty(r "/trigger_string") == "s") ? V[r "/trigger_string"] : ""
      hm = (ty(r "/modem") == "o"); mdm = index(tr, "mdm") > 0
      if (!mdm && !hm) continue
      sc = hm * 4 + (index(tr, "uid|is|") > 0) * 2 + mdm
      if (sc > best) { best = sc; bi = U[x] }
    }
    if (bi == "") exit
    r = "D/" bi "/modem"; man = str(r "/manual_apn"); md = tolower(str(r "/apn_mode"))
    if (md == "default" || md == "auto") md = "auto"; else if (md == "") md = (man != "") ? "manual" : "auto"
    print bi "\t" md "\t" (md == "manual" ? man : "")
  }'
}

# /api/status/gps payload → f.lat/f.lon/f.acc (DMS objects, as the CBA850 answers, or decimal).
rt_cp_gps() {
  awk -F "$RT_TAB" "$RT_LIB"'
  function dms(p, d, neg) { d = num(p "/degree"); if (d == "") return ""
    neg = (d < 0 || V[p "/degree"] ~ /^[ ]*-0*\.[0-9]*[ ]*$/); d = (d < 0 ? -d : d) + num0(p "/minute") / 60 + num0(p "/second") / 3600
    return neg ? -d : d }
  function num0(p, v) { v = num(p); return v == "" ? 0 : (v < 0 ? -v : v) }
  function first(a, b, c) { return has(a) ? a : has(b) ? b : c }
  END {
    f = has("D/data/fix") ? "D/data/fix" : has("D/fix") ? "D/fix" : has("D/data") ? "D/data" : "D"
    if (ty(f) != "o") exit
    if (ty(f "/latitude") == "o") { la = dms(f "/latitude"); lo = dms(f "/longitude") }
    else { la = num(first(f "/latitude", f "/lat")); lo = num(first(f "/longitude", f "/lon", f "/lng")) }
    rt_fix(la, lo, num(f "/accuracy"))
  }
  function rt_fix(la, lo, ac) {
    if (la == "" || lo == "" || la > 90 || la < -90 || lo > 180 || lo < -180 || (la == 0 && lo == 0)) return
    print "f.lat\t" nf(la); print "f.lon\t" nf(lo); if (ac != "") print "f.acc\t" ac }'
}

# --- Peplink parsers (PURE; stdin = payload flat under D) ---------------------------------------
# Fixtures = the app's peplink.test.ts via peplink.rs, plus the owner's Balance One on 8.5.5.

# Is this flat failure body "you are not signed in"? (peplink.rs session_expired)
rt_pl_expired() {
  awk -F "$RT_TAB" "$RT_LIB"'END { m = tolower(str("message"))
    exit !(ty("") == "o" && str("stat") == "fail" && (num("code") == 401 || m ~ /unauthori|login|session/)) }'
}

# status.system.info → p.* (fields flat, or one level under device/system/info), or none.
rt_pl_probe() {
  awk -F "$RT_TAB" "$RT_LIB"'
  END {
    t = (ty("D/response") == "o") ? "D/response" : "D"; if (ty(t) != "o") exit
    r = (ty(t "/device") == "o") ? t "/device" : (ty(t "/system") == "o") ? t "/system" : (ty(t "/info") == "o") ? t "/info" : t
    md = str(r "/productName"); if (md == "") md = str(r "/model")
    fw = str(r "/firmwareVersion"); if (fw == "") fw = str(r "/firmware")
    mc = str(r "/mac")
    if (md == "" && fw == "" && mc == "") exit
    print "p._\t1"; o("p.model", md); o("p.firmware", fw); o("p.mac", mc); o("p.serial", str(r "/serialNumber"))
  }'
}

# info.firmware (documented since 7.1.1, answers before sign-in) → the in-use image's version.
rt_pl_firmware() {
  awk -F "$RT_TAB" "$RT_LIB"'
  END { t = (ty("D/response") == "o") ? "D/response" : "D"; if (ty(t) != "o") exit
    m = kids(t, U, 1)
    for (x = 1; x <= m; x++) { e = t "/" U[x]
      if (U[x] != "order" && ty(e) == "o" && ty(e "/inUse") == "b" && V[e "/inUse"] == "true") { v = str(e "/version"); if (v != "") print v; exit } } }'
}

# status.wan.connection → m.* (the cellular block, if any) and w.* (the active uplink).
rt_pl_status() {
  awk -F "$RT_TAB" "$RT_LIB"'
  function up(w) { return str(w "/statusLed") == "green" || (ty(w "/message") == "s" && tolower(V[w "/message"]) ~ /^connected/) }
  END {
    t = (ty("D/response") == "o") ? "D/response" : "D"; if (ty(t) != "o") exit
    m = kids(t, U, 1); ent = 0; wp = ""; cp = ""
    for (x = 1; x <= m; x++) { e = t "/" U[x]; if (U[x] == "order" || ty(e) != "o") continue
      ent++; if (wp == "" && up(e)) wp = e; if (cp == "" && ty(e "/cellular") == "o") cp = e }
    if (cp != "") {
      c = cp "/cellular"
      sm = str(c "/simStatus"); if (sm == "") sm = str(c "/sim"); sm = tolower(sm)
      print "m.sim\t" ((sm == "sim card is ready" || sm == "ready") ? "ok" : (sm == "no sim card detected" || sm == "no sim") ? "missing" : (sm == "sim card is locked" || sm == "locked") ? "locked" : "unknown")
      o("m.carrier", str(c "/carrier"), str(c "/operator"))
      o("m.mode", str(c "/dataTechnology"), str(c "/network"))
      sg = has(c "/signal") ? c "/signal" : c "/signalLevel"; if (ty(sg) != "o") sg = ""
      split("rssi rsrp rsrq sinr", ks, " ")
      for (x = 1; x <= 4; x++) o("m." ks[x], num((sg != "" && has(sg "/" ks[x])) ? sg "/" ks[x] : c "/" ks[x]))
      print "m.connected\t" (up(cp) ? 1 : 0)
      o("m.ip", str(cp "/ip"))
    }
    if (ent) {
      if (wp == "") { print "w.wan\tnone"; print "w.up\t0" }
      else { tp = tolower(str(wp "/type")); print "w.wan\t" (tp ~ /cellular|modem/ ? "lte" : tp ~ /wifi/ ? "repeater" : "wired"); print "w.up\t1"; o("w.ip", str(wp "/ip")) }
    }
  }'
}

# info.location → gpsEnabled (the unit`s own `gps` flag) and f.* when it has a lock.
rt_pl_gps() {
  awk -F "$RT_TAB" "$RT_LIB"'
  function first(a, b, c) { return has(a) ? a : has(b) ? b : c }
  END {
    t = (ty("D/response") == "o") ? "D/response" : "D"
    if (ty(t "/gps") == "b") print "gpsEnabled\t" (V[t "/gps"] == "true" ? 1 : 0)
    l = has("D/response/location") ? "D/response/location" : has("D/response") ? "D/response" : has("D/location") ? "D/location" : "D"
    if (ty(l) != "o") exit
    la = num(first(l "/latitude", l "/lat")); lo = num(first(l "/longitude", l "/lon", l "/lng")); ac = num(l "/accuracy")
    if (la == "" || lo == "" || la > 90 || la < -90 || lo > 180 || lo < -180 || (la == 0 && lo == 0)) exit
    print "f.lat\t" la; print "f.lon\t" lo; if (ac != "") print "f.acc\t" ac
  }'
}

# --- Measurement (PURE) -------------------------------------------------------------------------

# Snapshot lines on stdin → the `modem.measurement` query string, THE SAME NAMES AND ORDER as
# routers.rs modem_params (and so as push_modem). $1 = the plan-burn KB delta or "". `av` is
# `hub-lite-<version>`, not the daemon's `hub-<version>`: the fleet console steers rollouts by it, and
# a hub-lite claiming a daemon version would be steered by the wrong one.
rt_params() {
  awk -F "$RT_TAB" -v kb="${1:-}" -v av="hub-lite-${HUB_LITE_VERSION:-0}" '
  BEGIN { for (c = 1; c < 256; c++) ORD[sprintf("%c", c)] = c }
  function enc(x, r, ch, j) { r = ""; for (j = 1; j <= length(x); j++) { ch = substr(x, j, 1); r = r (ch ~ /[A-Za-z0-9._~-]/ ? ch : sprintf("%%%02X", ORD[ch])) } return r }
  function nm(x) { return (x == int(x)) ? sprintf("%d", x) : sprintf("%.1f", x) }
  function put(k, v) { if (v != "") q = q "&" k "=" enc(v) }
  { e = index($0, "\t"); if (e) S[substr($0, 1, e - 1)] = substr($0, e + 1) }
  END {
    if (!("m.sim" in S)) exit
    q = "up=" (S["m.connected"] == 1 ? 1 : 0)
    put("mode", S["m.mode"])
    split("rssi rsrp sinr rsrq", ks, " ")
    for (x = 1; x <= 4; x++) if (("m." ks[x]) in S) put(ks[x], nm(S["m." ks[x]] + 0))
    put("carrier", S["m.carrier"]); put("sim", S["m.sim"])
    if (("m.txBytes" in S) && ("m.rxBytes" in S)) put("dataMb", sprintf("%.0f", int((S["m.txBytes"] + S["m.rxBytes"]) / 1048576)))
    put("wan", S["w.wan"]); put("ip", ("m.ip" in S) ? S["m.ip"] : S["w.ip"])
    put("model", S["p.model"]); put("fw", S["p.firmware"]); put("av", av)
    if (kb != "" && kb > 0) put("wanKb_cellular", kb)
    print q
  }'
}

# "prev_tx prev_rx" "tx rx" → KB since the last poll, or nothing: no earlier sample, or a counter
# that went BACKWARDS (a reboot zeroes it — charging the whole new total would bill phantom bytes).
rt_kb_delta() {
  [ -n "$1" ] || return 0
  awk -v p="$1" -v c="$2" 'BEGIN { split(p, a, " "); split(c, b, " ")
    if (b[1] + 0 < a[1] + 0 || b[2] + 0 < a[2] + 0) exit
    printf "%.0f\n", int(((b[1] - a[1]) + (b[2] - a[2])) / 1024) }'
}

# --- JSON out (PURE) ----------------------------------------------------------------------------

# Snapshot lines → the daemon's Snapshot / ProbeBody members, in serde's field order.
RT_J='
function q(x) { gsub(/\\/, "\\\\&", x); gsub(/"/, "\\\"", x); gsub(/[\001-\037]/, "", x); return "\"" x "\"" }
function jv(x, t) { return t == "n" ? x : t == "b" ? (x == 1 ? "true" : "false") : q(x) }
function jo(pre, spec, mark, n, a, i, nm, t, r) {
  if (!((pre mark) in S)) return ""
  n = split(spec, a, " "); r = ""
  for (i = 1; i <= n; i++) { t = substr(a[i], 1, 1); nm = substr(a[i], 3); if ((pre nm) in S) r = r (r == "" ? "" : ",") q(nm) ":" jv(S[pre nm], t) }
  return "{" r "}" }
function add(k, v) { if (v != "") R = R (R == "" ? "" : ",") q(k) ":" v }
function members(st) {
  if (st) { add("atMs", ("atMs" in S) ? S["atMs"] : 0); if ("okAtMs" in S) add("okAtMs", S["okAtMs"]) }
  add("probe", jo("p.", "s:model s:firmware s:mac s:serial", "_"))
  add("modem", jo("m.", "s:sim s:carrier s:mode n:rssi n:rsrp n:rsrq n:sinr b:connected s:ip n:txBytes n:rxBytes", "sim"))
  add("wan", jo("w.", "s:wan b:up s:ip", "wan"))
  if ("gpsEnabled" in S) add("gpsEnabled", jv(S["gpsEnabled"], "b"))
  add("fix", jo("f.", "n:lat n:lon n:acc", "lat"))
  if (st) { add("apn", jo("a.", "s:mode s:apn", "mode")); if ("error" in S) add("error", q(S["error"])) } }
function sline(l, e) { e = index(l, "\t"); if (e) S[substr(l, 1, e - 1)] = substr(l, e + 1) }
'

# One stored router line (on $1) → the daemon's RouterStatus: config WITHOUT the password or token
# (only whether each is set), plus the last snapshot as `state` when there is one.
rt_router_json() {
  printf '%s\n' "$1" | awk -F "$RT_FS" -v D="$RT_DIR" -v CP="$RT_CAPS_CP" -v PL="$RT_CAPS_PL" "$RT_J"'
  { R = ""; add("id", q($1)); add("vendor", q($2)); add("name", q($3)); add("host", q($4)); add("port", ($5 + 0) ? $5 + 0 : 443)
    add("username", q($6 == "" ? "admin" : $6)); add("hasPassword", $7 == "" ? "false" : "true"); add("agentEnrolled", $8 == "" ? "false" : "true")
    add("gpsEnabled", $9 == 1 ? "true" : "false"); add("gpsDevId", q($10)); p = $11 + 0; add("pollSecs", p == 0 ? 120 : p < 30 ? 30 : p)
    add("enabled", $12 == 1 ? "true" : "false"); add("capabilities", $2 == "peplink" ? PL : CP)
    f = D "/" $1 ".snap"; got = 0
    while ((getline l < f) > 0) { sline(l); got = 1 }
    close(f)
    if (got) { keep = R; R = ""; members(1); st = R; R = keep; add("state", "{" st "}") }
    print "{" R "}" }'
}

# The routers array for GET /api/hub/routers and /status.
rt_list_json() {
  _rtl=""
  if [ -s "$RT_CONF" ]; then
    while IFS= read -r _rtline; do
      [ -n "$_rtline" ] && _rtl="${_rtl:+$_rtl,}$(rt_router_json "$_rtline")"
    done < "$RT_CONF"
  fi
  printf '[%s]' "$_rtl"
}

# Capability self-test: this file loaded, curl present, and the tokenizer answers a known shape.
rt_ok() {
  command -v curl >/dev/null 2>&1 || return 1
  [ "$(printf '{"a":[1]}' | rt_flat | tail -n 1)" = "a/0${RT_TAB}n${RT_TAB}1" ]
}

# --- Transport ----------------------------------------------------------------------------------

rt_cq() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }   # a curl-config (or JSON) string body

# PURE: Peplink admin is https (self-signed) unless pointed at :80; port 0 means 443.
# RT_PL_BASE is test.sh's seam (peplink.rs at_base): a stub router serves plain http on a high port.
rt_pl_base() {
  [ -n "${RT_PL_BASE:-}" ] && { printf '%s' "$RT_PL_BASE"; return; }
  _rtp="${2:-0}"; [ "$_rtp" = "0" ] && _rtp=443
  case "$_rtp" in 80) printf 'http://%s' "$1" ;; 443) printf 'https://%s' "$1" ;; *) printf 'https://%s:%s' "$1" "$_rtp" ;; esac
}

# Per-request curl bounds. The DEFAULT is the interactive door's (/api/hub/routers: probe, add,
# refresh, apn, gps, password, reboot, read): an app or relay request waits on that answer — a probe
# is up to five requests inside one CGI run under uhttpd's script timeout — so it keeps 15 s / 5 s.
# The BACKGROUND poll (rt_poll_report, owner ruling 2026-09-17) raises both: a slow NCOS or Peplink
# API answer is not a dead router, and no one is waiting on it. 30 s is the daemon's 0.3.52 value.
RT_MAX_TIME=15
RT_CONNECT_TIMEOUT=5
RT_POLL_MAX_TIME=30
RT_POLL_CONNECT_TIMEOUT=15

# $1 method, $2 url, $3 extra curl-config lines. Body → $RT_W/b, headers → $RT_W/h, status → RT_CODE.
# ⚠️ `insecure`: an NCOS or Peplink answers its LAN on 443 with a factory self-signed certificate
# and there is no CA aboard to vouch for it — the daemon's lan_client does the same, and only for an
# address the owner typed in. Everything goes to curl on STDIN, so no credential is ever in argv.
rt_req() {
  : > "$RT_W/b"; : > "$RT_W/h"
  RT_CODE=$({ printf 'url = "%s"\nrequest = "%s"\nsilent\ninsecure\nmax-time = %s\nconnect-timeout = %s\noutput = "%s/b"\ndump-header = "%s/h"\nwrite-out = "%%{http_code}"\n' \
    "$(rt_cq "$2")" "$1" "$RT_MAX_TIME" "$RT_CONNECT_TIMEOUT" "$RT_W" "$RT_W"; printf '%s\n' "${3:-}"; } | curl -K - 2>/dev/null)
  _rtrc=$?
  case "$RT_CODE" in ''|000)
    case "$_rtrc" in
      28) RT_ERR="the router did not answer (timed out) — is the hub on the same network?" ;;
      6|7) RT_ERR="the router could not be reached at that address" ;;
      *) RT_ERR="the router could not be reached (curl rc $_rtrc)" ;;
    esac
    return 1 ;;
  esac
  rt_flat < "$RT_W/b" > "$RT_W/f"
  return 0
}

# NCOS: Basic auth on every request (there is no session). $1 method, $2 path, $3 JSON for a PUT
# (sent as the form field `data=<json>`). Payload → RT_OUT (flat under D).
rt_ncos() {
  _rtx="user = \"$(rt_cq "${RT_USER:-admin}:$RT_PASS")\""
  [ -n "${3:-}" ] && _rtx="$_rtx
data-urlencode = \"$(rt_cq "data=$3")\""
  rt_req "$1" "$(cradlepoint_base "$RT_HOST" "$RT_PORT")$2" "$_rtx" || return 1
  case "$RT_CODE" in
    401|403) RT_ERR="$RT_REFUSED"; return 1 ;;
    2??) ;;
    *) RT_ERR="the router answered HTTP $RT_CODE"; return 1 ;;
  esac
  rt_env ncos
}

rt_env() {
  RT_OUT=$(rt_unwrap "$1" < "$RT_W/f")
  case "$RT_OUT" in '!'*) RT_ERR="${RT_OUT#!}"; return 1 ;; esac
  return 0
}

# Peplink: POST /api/login → the cookie session. A refused sign-in is the one error an owner can fix,
# so it is named; the credential never appears in any message.
rt_pl_login() {
  _rtu=$(rt_cq "${RT_USER:-admin}"); _rtpw=$(rt_cq "$RT_PASS")
  _rtx='{"username":"'"$_rtu"'","password":"'"$_rtpw"'"}'
  _rtx=$(rt_cq "$_rtx")
  rt_req POST "$(rt_pl_base "$RT_HOST" "$RT_PORT")/api/login" "header = \"Content-Type: application/json\"
data-binary = \"$_rtx\"" || return 1
  _rtx=""; _rtpw=""
  case "$RT_CODE" in
    401|403) RT_ERR="$RT_REFUSED"; return 1 ;;
    2??) ;;
    *) RT_ERR="the router answered HTTP $RT_CODE"; return 1 ;;
  esac
  RT_COOKIE=$(tr -d '\r' < "$RT_W/h" | awk 'tolower($0) ~ /^set-cookie:/ { v = substr($0, index($0, ":") + 1); sub(/;.*/, "", v); gsub(/^[ \t]+|[ \t]+$/, "", v); if (v != "") r = r (r == "" ? "" : "; ") v } END { print r }')
  if ! rt_env peplink; then
    case "$RT_OUT" in '!the router did not answer with JSON') ;; *) RT_ERR="the router refused the sign-in — $RT_ERR" ;; esac
    return 1
  fi
  RT_SESSION=1
}

# One GET with the session: 0 ok (flat in $RT_W/f), 1 error (RT_ERR), 2 "not signed in (any more)".
rt_pl_once() {
  _rtx=""; [ -n "${RT_COOKIE:-}" ] && _rtx="header = \"Cookie: $(rt_cq "$RT_COOKIE")\""
  rt_req GET "$(rt_pl_base "$RT_HOST" "$RT_PORT")$1" "$_rtx" || return 1
  case "$RT_CODE" in
    401|403) return 2 ;;
    2??) ;;
    *) RT_ERR="the router answered HTTP $RT_CODE"; return 1 ;;
  esac
  grep -q "^${RT_TAB}o" "$RT_W/f" && ! grep -q "^${RT_TAB}!" "$RT_W/f" || { RT_ERR="the router did not answer with JSON"; return 1; }
  rt_pl_expired < "$RT_W/f" && return 2
  return 0
}

# GET a status path; payload → RT_OUT. Signs in when there is no session, and once more when the
# router says the session is gone — a session expired between polls costs a round trip, not a read.
rt_pl_get() {
  [ -n "${RT_SESSION:-}" ] || rt_pl_login || return 1
  rt_pl_once "$1"; _rtr=$?
  if [ "$_rtr" = 2 ]; then
    rt_pl_login || return 1
    rt_pl_once "$1"; _rtr=$?
    [ "$_rtr" = 2 ] && { RT_ERR="$RT_REFUSED"; return 1; }
  fi
  [ "$_rtr" = 0 ] || return 1
  rt_env peplink
}

# --- The vendor door ----------------------------------------------------------------------------
# Every function below reads the router from RT_VENDOR RT_HOST RT_PORT RT_USER RT_PASS and prints
# normalized snapshot lines to RT_NEW. `unsupported` mirrors routers.rs Driver::unsupported_action.

rt_unsupported() {
  case "$RT_VENDOR:$1" in
    peplink:apn) _rtw="reading or setting the APN" ;;
    peplink:reboot) _rtw="rebooting" ;;
    peplink:read) _rtw="the read diagnostic" ;;
    *) return 1 ;;
  esac
  RT_ERR="$_rtw is not supported on a Peplink through the hub — use the device's own app or admin pages"
}

# Prove the sign-in and read identity.
rt_probe() {
  case "$RT_VENDOR" in
    cradlepoint)
      rt_ncos GET /api/status/product_info || return 1
      _rtpi="$RT_OUT"
      rt_ncos GET /api/status/fw_info && _rtpi="$_rtpi
$(printf '%s\n' "$RT_OUT" | sed 's/^D/F/')"
      RT_NEW=$(printf '%s\n' "$_rtpi" | rt_cp_probe)
      [ -n "$RT_NEW" ] || { RT_ERR="the router answered, but its model could not be read"; return 1; }
      ;;
    peplink)
      # Firmware 8.5 publishes no model endpoint (#146): identity is best-effort, the SIGN-IN is what
      # must be proven — through the documented status.wan.connection, firmware from info.firmware.
      if rt_pl_get /api/status.system.info; then
        RT_NEW=$(printf '%s\n' "$RT_OUT" | rt_pl_probe)
        [ -n "$RT_NEW" ] && return 0
      else
        case "$RT_ERR" in "the router refused the sign-in"*|*"could not be reached"*|*"did not answer (timed out)"*) return 1 ;; esac
      fi
      rt_pl_get /api/status.wan.connection || return 1
      RT_NEW="p._${RT_TAB}1"
      if rt_pl_get /api/info.firmware; then
        _rtfw=$(printf '%s\n' "$RT_OUT" | rt_pl_firmware)
        [ -n "$_rtfw" ] && RT_NEW="$RT_NEW
p.firmware${RT_TAB}$_rtfw"
      fi
      ;;
  esac
  return 0
}

# The status side — one request on either vendor.
rt_status() {
  case "$RT_VENDOR" in
    cradlepoint) rt_ncos GET /api/status/wan/devices || return 1; RT_NEW=$(printf '%s\n' "$RT_OUT" | rt_cp_status) ;;
    peplink) rt_pl_get /api/status.wan.connection || return 1; RT_NEW=$(printf '%s\n' "$RT_OUT" | rt_pl_status) ;;
  esac
}

# The GPS side, best-effort ($1 = 1 when the owner wants the fix). A Peplink has no switch to read;
# its `gps` flag arrives with the location, and no request is made when the owner did not ask.
rt_gps() {
  RT_NEW=""
  case "$RT_VENDOR" in
    cradlepoint)
      if rt_ncos GET /api/config/system/gps/enabled; then
        case "$RT_OUT" in "D${RT_TAB}b${RT_TAB}true") RT_NEW="gpsEnabled${RT_TAB}1" ;; "D${RT_TAB}b${RT_TAB}false") RT_NEW="gpsEnabled${RT_TAB}0" ;; esac
      fi
      [ "$1" = 1 ] && rt_ncos GET /api/status/gps && RT_NEW="$RT_NEW
$(printf '%s\n' "$RT_OUT" | rt_cp_gps)"
      ;;
    peplink) [ "$1" = 1 ] && rt_pl_get /api/info.location && RT_NEW=$(printf '%s\n' "$RT_OUT" | rt_pl_gps) ;;
  esac
  return 0
}

rt_set_gps() {
  [ "$RT_VENDOR" = cradlepoint ] || return 0
  _rtb=false; [ "$1" = 1 ] && _rtb=true
  rt_ncos PUT /api/config/system/gps/enabled "$_rtb"
}

rt_apn() {
  rt_ncos GET /api/config/wan/rules2 || return 1
  RT_APN=$(printf '%s\n' "$RT_OUT" | rt_cp_apn)
  [ -n "$RT_APN" ] || { RT_ERR="the router reports no cellular WAN rule to read an APN from"; return 1; }
}

# One PUT per LEAF, never the `modem` object: a PUT of {"apn_mode":…} to …/modem came back
# success:false on the CBA850 (bench 2026-09-12). The name goes FIRST, so the mode flip never points
# at an empty name; automatic is written as NCOS's `default`.
rt_set_apn() {
  rt_apn || return 1
  _rtm="/api/config/wan/rules2/${RT_APN%%"$RT_TAB"*}/modem"
  if [ "$1" = manual ]; then
    _rta=$(printf '%s' "$2" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
    [ -n "$_rta" ] || { RT_ERR="an APN name is required for manual mode"; return 1; }
    rt_ncos PUT "$_rtm/manual_apn" "\"$(rt_cq "$_rta")\"" || return 1
    rt_ncos PUT "$_rtm/apn_mode" '"manual"' || return 1
  else
    rt_ncos PUT "$_rtm/apn_mode" '"default"' || return 1
  fi
  rt_apn
}

rt_apn_json() {
  _rtam=${RT_APN#*"$RT_TAB"}; _rtan=${_rtam#*"$RT_TAB"}; _rtam=${_rtam%%"$RT_TAB"*}
  if [ "$_rtam" = manual ] && [ -n "$_rtan" ]; then
    printf '{"mode":"manual","apn":"%s"}' "$(rt_cq "$_rtan")"
  else
    printf '{"mode":"%s"}' "$(rt_cq "$_rtam")"
  fi
}

# --- Stored routers + snapshots -----------------------------------------------------------------
# $RT_CONF, one router per line, fields split by \037 (never whitespace: `read` would collapse the
# empty fields an unset username or token leaves):
#   id vendor name host port username password agentToken gpsEnabled gpsDevId pollSecs enabled

rt_load() {
  [ -s "$RT_CONF" ] || return 1
  while IFS="$RT_FS" read -r RT_ID RT_VENDOR RT_NAME RT_HOST RT_PORT RT_USER RT_PASS RT_TOKEN RT_GPS RT_GPSDEV RT_POLL RT_EN; do
    [ "$RT_ID" = "$1" ] && return 0
  done < "$RT_CONF"
  return 1
}

rt_save() {
  _rtt="$RT_CONF.$$"
  (
    umask 077
    { [ -f "$RT_CONF" ] && awk -F "$RT_FS" -v id="$RT_ID" '$1 != id' "$RT_CONF"
      printf '%s\037%s\037%s\037%s\037%s\037%s\037%s\037%s\037%s\037%s\037%s\037%s\n' "$RT_ID" "$RT_VENDOR" "$RT_NAME" "$RT_HOST" \
        "${RT_PORT:-0}" "$RT_USER" "$RT_PASS" "$RT_TOKEN" "${RT_GPS:-0}" "$RT_GPSDEV" "${RT_POLL:-0}" "${RT_EN:-1}"
    } > "$_rtt"
  ) && chmod 600 "$_rtt" && mv "$_rtt" "$RT_CONF"
}

rt_forget() {
  [ -f "$RT_CONF" ] || return 1
  awk -F "$RT_FS" -v id="$1" '$1 == id { f = 1 } END { exit !f }' "$RT_CONF" || return 1
  (umask 077; awk -F "$RT_FS" -v id="$1" '$1 != id' "$RT_CONF" > "$RT_CONF.$$") && mv "$RT_CONF.$$" "$RT_CONF"
  rm -f "$RT_DIR/$1".*
}

rt_poll_secs() { _rtps=$(( ${1:-0} + 0 )); [ "$_rtps" = 0 ] && _rtps=120; [ "$_rtps" -lt 30 ] && _rtps=30; echo "$_rtps"; }

# Replace snapshot keys matching the ERE $1 with the lines on stdin (atomic).
rt_snap() {
  _rtf="$RT_DIR/$RT_ID.snap"
  mkdir -p "$RT_DIR" && chmod 700 "$RT_DIR"
  { [ -f "$_rtf" ] && grep -vE "^($1)" "$_rtf"; grep -v '^$'; } > "$_rtf.$$" 2>/dev/null
  mv "$_rtf.$$" "$_rtf"
}

rt_work() { RT_W="$RT_DIR/w.$$"; mkdir -p "$RT_W" && chmod 700 "$RT_DIR" "$RT_W"; RT_SESSION=""; RT_COOKIE=""; }

# One full read — the poll loop's unit of work and the `refresh` action. On failure the last-known
# state is kept and only `error` changes, so a router that just went dark still shows its identity.
rt_poll() {
  _rtnow="$(date +%s)000"
  if ! rt_status; then
    printf 'atMs\t%s\nerror\t%s\n' "$_rtnow" "$RT_ERR" | rt_snap "atMs$RT_TAB|error$RT_TAB"
    return 1
  fi
  _rtnew="$RT_NEW"
  if ! grep -q '^p\._' "$RT_DIR/$RT_ID.snap" 2>/dev/null; then
    rt_probe && _rtnew="$_rtnew
$RT_NEW"
  fi
  rt_gps "${RT_GPS:-0}"
  printf '%s\n%s\natMs\t%s\nokAtMs\t%s\n' "$_rtnew" "$RT_NEW" "$_rtnow" "$_rtnow" \
    | rt_snap "m\\.|w\\.|f\\.|gpsEnabled$RT_TAB|atMs$RT_TAB|okAtMs$RT_TAB|error$RT_TAB"
}

# PURE: is this snapshot (stdin) a reading that reports the uplink DOWN — the up=0 the report would
# carry? With a modem, its connection (rt_params' up); without one, the WAN's own up flag. A snapshot
# that says neither is not a down reading.
rt_snap_down() {
  awk -F "$RT_TAB" '{ S[$1] = $2 } END { if ("m.sim" in S) exit !(S["m.connected"] != 1); if ("w.up" in S) exit !(S["w.up"] != 1); exit 1 }'
}

# Poll and report — in the background, off the main loop (see rt_tick). `modem.measurement` goes AS
# the router through send_event with the router's own token; the fix goes as the linked `brv_gps_…`
# device through the relay spool, because /api/agent lets only a `hub_` token vouch for a GPS source
# and a hub-lite is a `brv_net_` device — the batch door carries any device under the router's own.
#
# 🔴 0.18.1 POLL GRACE (owner ruling 2026-09-17, brvg-hub-lite.sh poll_grace). A failed poll and a read
# that reports the uplink down are both BAD SAMPLES, and neither reports anything down on its own:
#   * inside the 45 s grace the report (when the cadence sends one) is the LAST GOOD reading,
#     $RT_ID.good, unchanged — never the down reading, never a measurement built from the failed read;
#   * when the grace runs out, down is reported AT ONCE with one log line: a down READ reports itself,
#     a failed POLL reports the last good reading with up=0. With no good reading since the state dir
#     was made (boot), a failed poll sends nothing — what a failed poll always did;
#   * the first good sample reports up AT ONCE, and logs "reachable again" only if down was reported;
#   * while down, the down report repeats on the check-in cadence (a failed send is not lost);
#   * a failure is retried on poll_grace's schedule: $RT_ID.due is moved earlier (5, 10, 20, 40, 60 s).
# The LAN snapshot is untouched by the grace: it still records each poll's `error` as it happens.
rt_poll_report() {
  rt_work; trap 'rm -rf "$RT_W"' EXIT
  RT_MAX_TIME=$RT_POLL_MAX_TIME; RT_CONNECT_TIMEOUT=$RT_POLL_CONNECT_TIMEOUT
  _rtt0=$(date +%s)
  _rtok=1; rt_poll || _rtok=0
  rt_graced "$_rtok" "$_rtt0" "$(date +%s)"
}

# The report half, with the times injected (test.sh drives it directly). $1 1 = the poll succeeded,
# $2 the poll's start epoch, $3 the epoch it finished; RT_ERR holds the failure.
rt_graced() {
  _rtsnapf="$RT_DIR/$RT_ID.snap"; _rtgood="$RT_DIR/$RT_ID.good"; _rtok=$1; _rtnow=$3
  _rtbad=0
  if [ "$1" != 1 ] || rt_snap_down < "$_rtsnapf"; then _rtbad=1; fi
  # shellcheck disable=SC2046
  set -- $(poll_grace "$(cat "$RT_DIR/$RT_ID.grace" 2>/dev/null)" "$2" "$3" "$_rtbad" "$(rt_poll_secs "$RT_POLL")")
  _rtact=$1
  if [ "$4" = 0 ]; then rm -f "$RT_DIR/$RT_ID.grace"; else echo "$3 $4 $5" > "$RT_DIR/$RT_ID.grace"; fi
  [ "$_rtbad" = 1 ] && echo "$2" > "$RT_DIR/$RT_ID.due"
  [ "$_rtbad" = 0 ] && cp "$_rtsnapf" "$_rtgood"
  # What this sample reports, and whether it goes now (an event) or on the check-in cadence.
  _rtsrc="$_rtsnapf"; _rtforce=0; _rtdownq=0
  case "$_rtact" in
    up) rt_log "$RT_HOST '$RT_NAME' - reachable again"; _rtforce=1 ;;
    hold) [ -f "$_rtgood" ] || return 0; _rtsrc="$_rtgood" ;;
    down)
      if [ "$_rtok" = 1 ]; then
        rt_log "$RT_HOST '$RT_NAME' - uplink down for ${POLL_GRACE_SECS}s - reporting it down"
      else
        rt_log "$RT_HOST '$RT_NAME' - no answer for ${POLL_GRACE_SECS}s - reporting it down ($RT_ERR)"
        [ -f "$_rtgood" ] || return 0
        _rtsrc="$_rtgood"; _rtdownq=1
      fi
      _rtforce=1 ;;
    # Still down: a down read reports itself on the cadence, as ever; a poll that still fails repeats
    # the down report (last good, up=0) on the cadence, so a down report whose send failed is retried.
    still) [ "$_rtok" = 1 ] || { [ -f "$_rtgood" ] || return 0; _rtsrc="$_rtgood"; _rtdownq=1; } ;;
  esac
  _rtdue=1
  _rtlast=$(cat "$RT_DIR/$RT_ID.sent" 2>/dev/null | tr -cd '0-9')
  _rtcad=$(cat "$RT_DIR/cadence" 2>/dev/null | tr -cd '0-9')
  [ "$_rtforce" = 0 ] && [ -n "$_rtlast" ] && [ $(( _rtnow - _rtlast )) -lt $(( ${_rtcad:-900} - 5 )) ] && _rtdue=0
  _rtkb=""
  # Plan-burn counters only from a FRESH read: the last good copy's counters are old news.
  if [ "$_rtsrc" = "$_rtsnapf" ] && [ "$_rtdue" = 1 ]; then
    _rtctr=$(awk -F "$RT_TAB" '$1 == "m.txBytes" { t = $2 } $1 == "m.rxBytes" { r = $2 } END { if (t != "" && r != "") print t " " r }' "$_rtsnapf")
    if [ -n "$_rtctr" ]; then
      _rtkb=$(rt_kb_delta "$(cat "$RT_DIR/$RT_ID.ctr" 2>/dev/null)" "$_rtctr")
      echo "$_rtctr" > "$RT_DIR/$RT_ID.ctr"
    fi
  fi
  _rtq=""
  [ "$_rtdue" = 1 ] && _rtq=$(rt_params "$_rtkb" < "$_rtsrc")
  [ "$_rtdownq" = 1 ] && [ -n "$_rtq" ] && _rtq="up=0${_rtq#up=?}"
  if [ -n "$_rtq" ]; then
    if [ -z "$RT_TOKEN" ]; then
      [ -s "$RT_DIR/$RT_ID.tok" ] || { rt_log "$RT_HOST '$RT_NAME' - no agent token; status is local only until the app enrolls it"; echo 1 > "$RT_DIR/$RT_ID.tok"; }
    else
      # 🔴 THE REPLY'S SIDE EFFECTS ARE NOT OURS. A command queued for the ROUTER device (a
      # `reboot`) must never run on the hub-lite that happened to carry the report — so the send
      # runs in a subshell where the reply handlers are inert, and its ack list never leaks back.
      (
        # shellcheck disable=SC2030
        DEVICE_ID="$RT_ID"; DEVICE_TOKEN="$RT_TOKEN"; PENDING_ACK=""
        run_commands() { :; }; apply_anchor() { :; }; apply_watch() { :; }; apply_live_fields() { :; }
        lt_apply_profiles() { cat >/dev/null; }; lt_apply_allowed() { :; }
        send_event modem.measurement "$_rtq" && echo "$_rtnow" > "$RT_DIR/$RT_ID.sent"
      ) || true
    fi
  fi
  if [ "$_rtok" = 1 ] && [ "$RT_GPS" = 1 ] && [ -n "$RT_GPSDEV" ]; then
    _rtg=$(awk -F "$RT_TAB" '{ S[$1] = $2 } END { if ("f.lat" in S) { printf "lat=%.6f&lon=%.6f", S["f.lat"], S["f.lon"]; if ("f.acc" in S) printf "&acc=%.1f", S["f.acc"] } }' "$RT_DIR/$RT_ID.snap")
    # The fix by exception (0.17.0, §A7.2): every poll while a member watches (the cadence file says
    # 60), otherwise only the first fix and a move past the deadband (floor 25 m) that is also more
    # than twice the fix's accuracy — the hub-lite's own GPS rule, gps_should_send.
    if [ -n "$_rtg" ] && rt_gps_due "$_rtg" "${_rtcad:-900}"; then
      lt_spool "$RT_GPSDEV" gps.measurement "$_rtg"
      printf '%s\n' "$_rtg" > "$RT_DIR/$RT_ID.gpssent"
      : > "$RT_DIR/drain"
    fi
  fi
}

# Should this managed-router fix be spooled? $1 "lat=..&lon=..[&acc=..]" $2 the check-in cadence.
rt_gps_due() {
  [ "$2" -le 60 ] 2>/dev/null && return 0
  _rgl=$(cat "$RT_DIR/$RT_ID.gpssent" 2>/dev/null)
  [ -n "$_rgl" ] || return 0
  _rgv() { printf '%s' "$1" | tr '&' '\n' | sed -n "s/^$2=//p"; }
  (
    GPS_LAST_LAT=$(_rgv "$_rgl" lat); GPS_LAST_LON=$(_rgv "$_rgl" lon)
    gps_should_send "$(_rgv "$1" lat)" "$(_rgv "$1" lon)" "$(_rgv "$1" acc)" 0 0 0 0
  )
}

# The main loop's hook. Cheap and never blocking: a router that has stopped answering costs 30 s
# per request, and the loop it would stall also carries the valve volume cutoff — so each due router
# is read in its own background child (one at a time per router), and only the relay drain the
# children ask for runs here, where the batch sequence file has a single writer.
rt_tick() {
  [ -s "$RT_CONF" ] || return 0
  mkdir -p "$RT_DIR" 2>/dev/null && chmod 700 "$RT_DIR"
  if [ -f "$RT_DIR/drain" ]; then rm -f "$RT_DIR/drain"; drain_relay; fi
  if [ -f "$RT_DIR/wake" ]; then rm -f "$RT_DIR/wake" "$RT_DIR"/*.due; fi
  _rtn=$(date +%s)
  while IFS="$RT_FS" read -r _rti _rtv _rtd _rth _rtd _rtd _rtd _rtd _rtd _rtd _rtps _rten; do
    [ "$_rten" = 1 ] && [ -n "$_rth" ] && [ -n "$_rtv" ] || continue
    [ "$_rtn" -ge "$(cat "$RT_DIR/$_rti.due" 2>/dev/null || echo 0)" ] || continue
    _rtpid=$(cat "$RT_DIR/$_rti.pid" 2>/dev/null)
    [ -n "$_rtpid" ] && kill -0 "$_rtpid" 2>/dev/null && continue
    echo $(( _rtn + $(rt_poll_secs "$_rtps") )) > "$RT_DIR/$_rti.due"
    ( rt_load "$_rti" && rt_poll_report ) </dev/null >/dev/null &
    echo $! > "$RT_DIR/$_rti.pid"
  done < "$RT_CONF"
  unset _rtd
}

# The soonest managed-router read, for the main loop's nap ($1 now; empty with no enabled router). While
# a read is running, now + 5 s: a failed read moves its own due time earlier (the 5 s first retry) after
# the loop has already chosen how long to sleep.
rt_next_due() {
  [ -s "$RT_CONF" ] || return 0
  _rtbest=""
  while IFS="$RT_FS" read -r _rti _rtv _rtd _rth _rtd _rtd _rtd _rtd _rtd _rtd _rtd _rten; do
    [ "$_rten" = 1 ] && [ -n "$_rth" ] && [ -n "$_rtv" ] || continue
    _rtx=$(cat "$RT_DIR/$_rti.due" 2>/dev/null | tr -cd '0-9'); _rtx=${_rtx:-$1}
    _rtpid=$(cat "$RT_DIR/$_rti.pid" 2>/dev/null)
    [ -n "$_rtpid" ] && kill -0 "$_rtpid" 2>/dev/null && [ "$_rtx" -gt $(( $1 + 5 )) ] && _rtx=$(( $1 + 5 ))
    { [ -z "$_rtbest" ] || [ "$_rtx" -lt "$_rtbest" ]; } && _rtbest=$_rtx
  done < "$RT_CONF"
  unset _rtd
  printf '%s' "$_rtbest"
}

# --- The /api/hub/routers door (called by hub-lite-api.sh; uses its reply/fail/valid_*) -----------

# Parse the request body into BT_<key> (type) / BV_<key> (value) for its top-level scalar keys.
# Values are single-quoted for eval with every embedded quote closed and reopened; keys are letters.
rt_body() {
  RT_BAD=""
  _rtsh=$(printf '%s' "$1" | rt_flat | awk -F "$RT_TAB" '
    NR == 1 && ($1 != "" || $2 != "o") { bad = 1 }
    $1 == "" && $2 == "!" { bad = 1 }
    $1 ~ /^[A-Za-z]+$/ { v = $3; gsub(/\047/, "\047\"\047\"\047", v); print "BT_" $1 "=" $2 "; BV_" $1 "=\047" v "\047" }
    END { if (bad || NR == 0) print "RT_BAD=1" }')
  eval "$_rtsh"
  [ -z "$RT_BAD" ]
}

# A string field: 0 present (RT_V), 1 absent or null. Any other type is the 422 serde would give.
rt_s() {
  eval "_rtt=\${BT_$1:-}; RT_V=\${BV_$1:-}"
  case "$_rtt" in s) return 0 ;; ''|z) RT_V=""; return 1 ;; esac
  fail 422 "invalid JSON body: $1 must be a string"
}
rt_n() {  # a non-negative integer no larger than $2
  eval "_rtt=\${BT_$1:-}; RT_V=\${BV_$1:-}"
  case "$_rtt" in ''|z) RT_V=""; return 1 ;; n) ;; *) fail 422 "invalid JSON body: $1 must be a number" ;; esac
  case "$RT_V" in ''|*[!0-9]*) fail 422 "invalid JSON body: $1 must be a whole number" ;; esac
  [ "${#RT_V}" -le 10 ] && [ "$RT_V" -le "$2" ] || fail 422 "invalid JSON body: $1 is out of range"
  RT_V=$((RT_V + 0))
}
rt_b() {
  eval "_rtt=\${BT_$1:-}; RT_V=\${BV_$1:-}"
  case "$_rtt" in b) [ "$RT_V" = true ] && RT_V=1 || RT_V=0; return 0 ;; ''|z) RT_V=""; return 1 ;; esac
  fail 422 "invalid JSON body: $1 must be true or false"
}
rt_trim() { printf '%s' "$1" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//'; }
# No control characters (the store is \037-separated and line-based) and bounded.
rt_clean() { [ "${#2}" -le "${3:-128}" ] && [ -z "$(printf '%s' "$2" | LC_ALL=C tr -d '\040-\176\200-\377')" ] || fail 422 "$1 is not valid"; }

rt_vendor() {
  case "$1" in cradlepoint|peplink) return 0 ;; esac
  fail 422 "this hub cannot manage a '$1' router yet"
}

rt_done_json() { rt_router_json "$(awk -F "$RT_FS" -v id="$RT_ID" '$1 == id' "$RT_CONF")"; }

rt_api() {
  rt_work; trap 'rm -rf "$RT_W"' EXIT
  _rta=""; rt_s action && _rta=$(printf '%s' "$RT_V" | tr 'A-Z' 'a-z' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
  _rtid=""; rt_s id && _rtid=$(rt_trim "$RT_V")
  # The id names files under $RT_DIR: nothing but the device-id alphabet ever reaches a path.
  [ -z "$_rtid" ] || valid_ident "$_rtid" || fail 422 "id is not a device id"
  case "$_rta" in
    probe)
      RT_VENDOR=cradlepoint; rt_s vendor && RT_VENDOR=$(rt_trim "$RT_V" | tr 'A-Z' 'a-z')
      rt_vendor "$RT_VENDOR"
      RT_HOST=""; rt_s host && RT_HOST=$(rt_trim "$RT_V")
      [ -n "$RT_HOST" ] || fail 422 "host is required"
      valid_host "$RT_HOST" || fail 422 "host must be an address or a host name"
      RT_PORT=0; rt_n port 65535 && RT_PORT=$RT_V
      RT_USER=""; rt_s username && RT_USER=$(rt_trim "$RT_V")
      RT_PASS=""; rt_s password && RT_PASS=$RT_V
      rt_clean username "$RT_USER"; rt_clean password "$RT_PASS"
      rt_probe || fail 502 "$RT_ERR"
      _rtpr="$RT_NEW"
      rt_status || RT_NEW=""
      _rtpr="$_rtpr
$RT_NEW"
      rt_gps 1
      _rtcaps=$RT_CAPS_CP; [ "$RT_VENDOR" = peplink ] && _rtcaps=$RT_CAPS_PL
      reply 200 "$(printf '%s\n%s\n' "$_rtpr" "$RT_NEW" | awk -v C="$_rtcaps" "$RT_J"'{ sline($0) } END { R = ""; members(0); add("capabilities", C); print "{" R "}" }')"
      ;;
    add)
      [ -n "$_rtid" ] || fail 422 "id is required"
      valid_ident "$_rtid" || fail 422 "id is not a device id"
      _rtold=0
      if rt_load "$_rtid"; then _rtold=1; else
        RT_ID="$_rtid"; RT_VENDOR=""; RT_NAME=""; RT_HOST=""; RT_PORT=0; RT_USER=""; RT_PASS=""; RT_TOKEN=""; RT_GPS=0; RT_GPSDEV=""; RT_POLL=0; RT_EN=1
      fi
      rt_s vendor && RT_VENDOR=$(rt_trim "$RT_V" | tr 'A-Z' 'a-z')
      [ -n "$RT_VENDOR" ] || RT_VENDOR=cradlepoint
      rt_vendor "$RT_VENDOR"
      rt_s name && RT_NAME=$(rt_trim "$RT_V")
      rt_s host && RT_HOST=$(rt_trim "$RT_V")
      rt_n port 65535 && RT_PORT=$RT_V
      rt_s username && RT_USER=$(rt_trim "$RT_V")
      rt_s password && [ -n "$RT_V" ] && RT_PASS=$RT_V
      rt_s agentToken && [ -n "$RT_V" ] && RT_TOKEN=$RT_V
      rt_b gpsEnabled && RT_GPS=$RT_V
      rt_s gpsDevId && RT_GPSDEV=$(rt_trim "$RT_V")
      rt_n pollSecs 4294967295 && RT_POLL=$RT_V
      rt_b enabled && RT_EN=$RT_V
      rt_clean name "$RT_NAME"; rt_clean username "$RT_USER"; rt_clean password "$RT_PASS"
      [ -n "$RT_HOST" ] || fail 422 "host is required"
      valid_host "$RT_HOST" || fail 422 "host must be an address or a host name"
      [ -z "$RT_TOKEN" ] || valid_token "$RT_TOKEN" || fail 422 "agentToken is not a device token"
      [ -z "$RT_GPSDEV" ] || valid_ident "$RT_GPSDEV" || fail 422 "gpsDevId is not a device id"
      [ -n "$RT_PASS" ] || fail 422 "the router's admin password is required"
      [ "$RT_GPS" = 1 ] && [ -z "$RT_GPSDEV" ] && fail 422 "gpsDevId (the brv_gps_… record) is required when gpsEnabled"
      # The sign-in is PROVED before anything is stored, so a typo cannot evict a working credential.
      rt_probe || fail 502 "$RT_ERR"
      _rtpr="$RT_NEW"
      if [ -z "$RT_NAME" ]; then
        RT_NAME=$(printf '%s\n' "$_rtpr" | sed -n "s/^p\\.model$RT_TAB//p")
        [ -n "$RT_NAME" ] || RT_NAME=Router
      fi
      if [ "$RT_GPS" = 1 ]; then rt_set_gps 1 || rt_log "$RT_HOST - could not switch the router's GPS on: $RT_ERR"; fi
      rt_save || fail 500 "this router cannot write its own configuration"
      _rtverb=added; [ "$_rtold" = 1 ] && _rtverb=updated
      _rtgw=off; [ "$RT_GPS" = 1 ] && _rtgw=on
      rt_log "$RT_VENDOR '$RT_NAME' at $RT_HOST $_rtverb (gps $_rtgw)"
      printf '%s\n' "$_rtpr" | rt_snap 'p\.'
      : > "$RT_DIR/wake"
      reply 200 "$(rt_done_json)"
      ;;
    remove)
      rt_forget "$_rtid" || fail 404 "no managed router with that id"
      rt_log "$_rtid removed"
      reply 200 '{"ok":true}'
      ;;
    refresh|read|apn|gps|password|reboot)
      rt_load "$_rtid" || fail 404 "no managed router with that id"
      rt_vendor "$RT_VENDOR"
      rt_unsupported "$_rta" && fail 422 "$RT_ERR"
      case "$_rta" in
        refresh)
          rt_poll && : > "$RT_DIR/wake"
          reply 200 "$(rt_done_json)"
          ;;
        apn)
          _rtmode=""; rt_s mode && _rtmode=$(rt_trim "$RT_V" | tr 'A-Z' 'a-z')
          if [ -z "$_rtmode" ]; then
            rt_apn || fail 502 "$RT_ERR"
          else
            case "$_rtmode" in auto|manual) ;; *) fail 422 "mode must be auto or manual" ;; esac
            rt_s apn; rt_clean apn "$RT_V"
            rt_set_apn "$_rtmode" "$RT_V" || fail 502 "$RT_ERR"
          fi
          _rtj=$(rt_apn_json)
          _rtam=${RT_APN#*"$RT_TAB"}
          printf 'a.mode\t%s\na.apn\t%s\n' "${_rtam%%"$RT_TAB"*}" "${_rtam#*"$RT_TAB"}" | grep -v "^a\\.apn$RT_TAB\$" | rt_snap 'a\.'
          reply 200 "$_rtj"
          ;;
        gps)
          _rton=1; rt_b gpsEnabled && _rton=$RT_V
          rt_s gpsDevId && RT_GPSDEV=$(rt_trim "$RT_V")
          [ -z "$RT_GPSDEV" ] || valid_ident "$RT_GPSDEV" || fail 422 "gpsDevId is not a device id"
          [ "$_rton" = 1 ] && [ -z "$RT_GPSDEV" ] && fail 422 "gpsDevId (the brv_gps_… record) is required when gpsEnabled"
          rt_set_gps "$_rton" || fail 502 "$RT_ERR"
          RT_GPS=$_rton
          rt_save || fail 500 "this router cannot write its own configuration"
          _rtgw=off; [ "$_rton" = 1 ] && _rtgw=on
          rt_log "$RT_HOST - GPS switched $_rtgw"
          : > "$RT_DIR/wake"
          [ "$_rton" = 1 ] && reply 200 '{"ok":true,"gpsEnabled":true}'
          reply 200 '{"ok":true,"gpsEnabled":false}'
          ;;
        password)
          rt_s password && [ -n "$RT_V" ] || fail 422 "password is required"
          RT_PASS=$RT_V
          rt_s username && RT_USER=$(rt_trim "$RT_V")
          rt_clean username "$RT_USER"; rt_clean password "$RT_PASS"
          rt_probe || fail 502 "$RT_ERR"
          rt_save || fail 500 "this router cannot write its own configuration"
          : > "$RT_DIR/wake"
          reply 200 '{"ok":true}'
          ;;
        reboot)
          rt_ncos PUT /api/control/system '{"reboot":true}' || fail 502 "$RT_ERR"
          rt_log "$RT_HOST - reboot requested"
          reply 200 '{"ok":true}'
          ;;
        read)
          # Read-only by construction (GET), status/config paths only, scrubbed, capped.
          _rtpath=""; rt_s path && _rtpath=$(rt_trim "$RT_V")
          case "$_rtpath" in /api/status|/api/config|/api/status/*|/api/config/*) ;; *) fail 422 "read takes an /api/status/… or /api/config/… path" ;; esac
          case "$_rtpath" in *..*|*//*|*\?*|*\#*|*/) fail 422 "that is not a plain path" ;; esac
          case "$_rtpath" in *[!A-Za-z0-9/_.-]*) fail 422 "a path is letters, digits, '/', '_', '-' and '.'" ;; esac
          rt_ncos GET "$_rtpath" || fail 502 "$RT_ERR"
          _rtroot=""; grep -q "^data$RT_TAB" "$RT_W/f" && _rtroot=data
          rt_flat J "$_rtroot" < "$RT_W/b" > "$RT_W/j"
          _rtsz=$(wc -c < "$RT_W/j" | tr -d ' ')
          [ "$_rtsz" -le "$RT_MAX_READ" ] || fail 502 "that path answers $_rtsz bytes — ask for a narrower one (limit $RT_MAX_READ)"
          reply 200 "{\"path\":\"$(rt_cq "$_rtpath")\",\"data\":$(cat "$RT_W/j")}"
          ;;
      esac
      ;;
    *) fail 422 "unknown action '$_rta'" ;;
  esac
}
