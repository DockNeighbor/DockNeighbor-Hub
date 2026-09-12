# BRVG phone-home hub-lite (Phase A)

One POSIX-shell hub-lite, two homes: a **GL.iNet router** (busybox ash, procd) and a **Raspberry
Pi-class hub** (systemd). It pushes GPS and modem telemetry **outbound** over HTTPS to the hosted
worker on a timer, so the vehicle reports without the app being onsite.

> ⚠️ **This heading used to say "Phase A = telemetry push; the Phase B command channel is
> deliberately not in this skeleton", and it was years of commits out of date.** It was read as
> current on 2026-08-31 and produced a wrong answer to the owner — that a hub-lite could only push
> telemetry and slam the valve shut on a flood. Not so. What is actually here:

**Present today**
- Telemetry push (GPS, modem) — the original Phase A.
- The **webhook receiver**: the vessel's Shellys report to the router, not the cloud, so a sensor's
  report never leaves the LAN.
- **Local flood shutoff** — `is_flood_shutoff` + `linktap_flood_close`, the safety path that must
  work with the uplink down.
- **Washdown and tank fill**, via `POST /api/hub/linktap/valve` — the daemon's own ValveReq shape.
  A washdown is time-only and a `volumeCapL` sent with it is REFUSED, exactly as the daemon
  refuses it. The run's own mode/duration/cap are recorded in its state file, so the next tick
  continues that cycle instead of adopting it into a Normal Run on the profile's cap.
- A **LinkTap cycle machine** (`lt_decide` + `lt_load_state`): normal runs, end-reason classification,
  restart-only-on-timer, adoption of external opens, and the **software volume cutoff** — which
  fires EARLY by the stop latency, mirroring the daemon's `cutoff_trigger_l`. On a hub-lite this
  cutoff is the only volume enforcement there is.
- A **LAN management door** (`hub-lite-mgmt.sh`, `?action=`) for status, lockdown and an
  allowlisted command set.
- The **`/api/hub/*` door** (`hub-lite-api.sh`) — `ping`, `status`, `linktap/state` — on port
  **8722**, the daemon's own contract, so the app has one hub client (owner ruling 2026-08-31:
  *"the hub-lite should move to 8722, keep one contract"*).

**Deliberately NOT here** (a full hub, or the app, does these)
- The washdown→Normal **handover** — the daemon reprograms the valve ~20s before a washdown
  expires so the water never stops. A hub-lite closes and reopens on its next tick instead, which
  is a visible gap in flow rather than a seamless swap. *Slower, not a different shape.*
- The daily usage ledger.
- The relay socket — a persistent WebSocket from busybox ash is not worth the overlay.
- Long-polling `/api/hub/linktap/state`: `wait` is accepted and ignored. Holding a request would
  hold a uhttpd worker AND a shell process on a box with 416 KB of free overlay. **A hub-lite is
  allowed to be slower; it is not allowed to be a different shape.**

Owner doctrine (2026-08-19): *"hub lite should do anything a hub can do as long as it is not
CPU/memory restrictive."* When adding to this list, say which side of that line the change sits on.

**Ports.** The receiver and both doors listen on **8722**. Port **8181** keeps answering as
`RECEIVER_LEGACY_PORT` because existing Shelly webhooks were registered against it — moving without
that would cut those sensors off from the local flood shutoff, and only the app can re-point them.
Set `RECEIVER_LEGACY_PORT=0` to retire it once nothing points there.

## How it reports

Standard telemetry webhooks — the exact path every Shelly on the vehicle already uses, so the
cloud side needs **zero changes**:

```
GET {WORKER_URL}/api/shelly?vid=…&device=…&event=gps.measurement&k=…&lat=…&lon=…&acc=…
GET {WORKER_URL}/api/shelly?vid=…&device=…&event=modem.measurement&k=…&rssi=…&rsrp=…&carrier=…&sim=…
```

Auth, preferred: a **per-device revocable token** (`DEVICE_TOKEN` → `GET /api/agent?...&t=`),
minted once by an admin via `POST /api/agent/enroll {vid, deviceId}` (Firebase ID token required;
re-enroll rotates, DELETE revokes). Legacy fallback when `DEVICE_TOKEN` is empty: the per-vehicle
webhook key (`VEHICLE_KEY` → `/api/shelly?...&k=`). Both land in the identical cloud pipeline.

## Install — from the app (the normal path)

Router panel → **☁️ Cloud reporting → Connect to cloud**. The desktop app mints this device's
token and installs the hub-lite onto the router over SSH using the admin password it already holds:
script, init service and the token-bearing config are written, the service is enabled, started and
verified. **No files are edited by hand.**

Availability: desktop (Tauri) builds only — the SSH client is deliberately kept out of the Android
binary (see `src-tauri/Cargo.toml`). Where it isn't available the app shows the manual steps below
instead of a button that cannot work. The eventual all-platform path is an **opkg feed**
(`plugins.install_package` exists in the 4.x RPC surface, so a signed .ipk installs with no SSH at
all) — that lands with the packaged hub-lite under the signed-artifact bar.

## Install — by hand (fallback / non-GL.iNet)

GL.iNet (tested platform: GL-X750, fw 4.3.28)

```sh
scp hub-lite/brvg-hub-lite.sh root@192.168.8.1:/usr/bin/brvg-hub-lite
scp hub-lite/brvg-hub-lite.conf.example root@192.168.8.1:/etc/brvg-hub-lite.conf
scp hub-lite/openwrt/etc/init.d/brvg-hub-lite root@192.168.8.1:/etc/init.d/brvg-hub-lite
ssh root@192.168.8.1 'chmod 755 /usr/bin/brvg-hub-lite /etc/init.d/brvg-hub-lite; chmod 600 /etc/brvg-hub-lite.conf'
# edit /etc/brvg-hub-lite.conf (VID, DEVICE_ID, VEHICLE_KEY), then:
ssh root@192.168.8.1 '/etc/init.d/brvg-hub-lite enable && /etc/init.d/brvg-hub-lite start; logread -f | grep brvg'
```

GPS/modem are read with AT commands straight to the modem port — the hub-lite runs as root
on-device, so no RPC login is involved. `AT_PORT` defaults to `/dev/ttyUSB2` (correct for the
X750's EC25). **X3000-class PCIe modems (RM520) expose a different device** — check
`ls /dev/mhi_* /dev/ttyUSB*` on the unit and set `AT_PORT`; expect `/dev/mhi_DUN`-style names
[unverified until an X3000 is on the bench].

## Install — Raspberry Pi hub

**One command**: `sudo sh hub-lite/hub/install.sh` (see [hub/README.md](hub/README.md) for what the
box is for and the hardware notes). The manual steps below are the same thing, unpacked.

```sh
sudo cp hub-lite/brvg-hub-lite.sh /usr/local/bin/brvg-hub-lite && sudo chmod 755 /usr/local/bin/brvg-hub-lite
sudo cp hub-lite/brvg-hub-lite.conf.example /etc/brvg-hub-lite.conf && sudo chmod 600 /etc/brvg-hub-lite.conf
sudo cp hub-lite/systemd/brvg-hub-lite.service /etc/systemd/system/
# edit /etc/brvg-hub-lite.conf, then:
sudo systemctl enable --now brvg-hub-lite && journalctl -fu brvg-hub-lite
```

### GPS on a router with no GPS antenna port

Some models (verified: **GL-X750** — two cellular SMA ports and nothing else) enable the modem's
GNSS happily and then see zero satellites forever, because the receiver has no antenna wired to it.
The fix is a **USB GPS dongle** (u-blox class, ~$15 — VK-162 / BU-353S4): plug it into the router's
USB port and the hub-lite finds it automatically. `GPS_SOURCE=auto` tries the modem first, then a USB
NMEA device (`/dev/ttyACM*`, and `ttyUSB` ports that are not the modem's AT port), then gpsd — the
ORDER matters, because a router with no GPS antenna answers the modem read forever with "no fix",
so the dongle must be tried even when the modem is present and healthy.

OpenWrt needs kernel modules before the dongle appears as a device. **Don't run those by hand** —
the package ships a setup script that installs them, finds the receiver, and reads it back to prove
it is really sending NMEA:

```sh
brvg-setup-usb-gps            # plug the dongle in first; idempotent, safe to re-run
brvg-setup-usb-gps --status   # what it found, and which GPS_SOURCE the hub-lite is set to
```

It installs `kmod-usb-acm` (u-blox and most CDC-ACM receivers) plus the FTDI/Prolific modules. That
is all that is needed — the hub-lite reads the device directly. It is NOT run at install time: the
dongle is normally plugged in later, and opkg needs the router online at the moment it runs.

Pin it explicitly with `GPS_SOURCE=nmea` + `GPS_DEVICE=/dev/ttyACM0` if auto-detection picks wrong.

Two network sources join the chain (2026-08-17, GPS parity with the hub — explicit choices, never
part of `auto`):

- **`GPS_SOURCE=tcp`** + `GPS_HOST`/`GPS_PORT` — NMEA 0183 served on the LAN (chartplotter, AIS,
  gpsd). The hub-lite dials in, reads a burst, and reuses the same RMC parser as the serial path.
  ⚠️ Uses busybox `nc`; verify it exists on FACTORY-STOCK firmware before shipping (the same trap
  as the manually-installed Lua on the bench box).
- **`GPS_SOURCE=cradlepoint`** + `CRADLEPOINT_HOST`/`_PORT`/`_USER`/`_PASSWORD` — poll a
  Cradlepoint NCOS router's local `/api/status/gps` over HTTP Basic auth (same env names as the
  TypeScript hub). The router is POLLED, never configured to push anywhere; the DMS payload shape
  is pinned to a bench capture (CBA850 fw 7.0.50).

### How the position actually reaches the app

**The hub-lite reads the dongle itself.** `GPS_SOURCE=auto` (the default the app writes) tries the
modem's GNSS, then a USB NMEA device, then gpsd, and pushes fixes outbound on its normal GPS tick.
There is nothing to serve and no port to open, and it works with nobody aboard.

⚠️ An earlier version of this doc described publishing the receiver over TCP with ser2net so the
desktop app could poll it. **That was the wrong design and has been removed** (2026-08-16). A USB
GPS is a serial device on the router; turning it into a network service added a daemon to install,
a port to open, a desktop-only dependency, and a second reader competing with the hub-lite for the same
device — to arrive at a position the hub-lite was already sending, and only while somebody was aboard.

If the app shows no position from the router, the order to check is: does `/dev/ttyACM0` exist
(`brvg-setup-usb-gps --status`), is the hub-lite running, and is `GPS_SOURCE` `auto` rather than `at`.

## Behavior

- GPS every `GPS_INTERVAL` (default 120 s, floor 30), modem every `MODEM_INTERVAL` (default
  600 s, floor 60). One small HTTPS GET per report — this rides the customer's metered link.
- GPS **report-by-exception**: the fix is collected and the anchor drag check run every
  `GPS_INTERVAL`, but the cloud SEND is skipped while parked — unless the fix moved past
  `GPS_DEADBAND_M` (default 50 m), an anchor watch is armed (always sends), or `GPS_LIVENESS_SECS`
  (default 1200 s / 20 min) has elapsed. `GPS_DEADBAND_M=0` disables it (send every tick). Keep the
  liveness floor under the vehicle's offline threshold (default 60 min) so a parked hub never reads
  as offline; a drag beyond the deadband, or an armed watch, always reports at full cadence.
- No fix → nothing sent (the cloud keeps last-known; "no fix" is diagnosed by the app's
  Health Check, not by telemetry spam). Failed sends are dropped; the next tick retries.
  Phase A is telemetry, not store-and-forward.
- Random start offset so a fleet doesn't tick in lockstep after a regional power event.

## Tests

`sh hub-lite/test.sh` — every parser is exercised with responses captured from the real GL-X750
bench session (2026-08-06) plus standard NMEA/gpsd shapes. Runs in CI (`hub-lite` job).

## Security posture (Phase A)

- Outbound HTTPS only; `WORKER_URL` must be https and defaults to the pinned first-party worker.
- The config file holds the vehicle key → `chmod 600`, root-owned.
- No inbound listener, no command execution, no self-update. Anything that installs or updates code
  on customer devices has to clear the full signed-artifact bar — a signed build, a checksum
  verified against the same release that produced it, and a documented rollback — and none of that
  exists here yet. This skeleton is installed manually, by us, on bench/dev hardware.


## Relay tier (X750-class routers) — roll up the Shellys' reports

`HUB_LITE_ENABLED=1` in `/etc/brvg-hub-lite.conf` turns this hub-lite into the **hub-lite tier** of the hub
architecture: a LAN-only uhttpd instance serves
`hub-lite-cgi.sh` at `http://<router>:8722/cgi-bin/report`, the Shellys' webhooks are re-registered
against it, and the hub-lite drains the spool into ONE `/api/agent/batch` report per modem interval.

Why: the metered link pays per TLS handshake, not per byte — one roll-up connection replaces one
connection per device per event. And once no sensor talks to the internet directly, lockdown's
forward chain can be deny-all with no allow rules at all.

Rules that hold regardless of settings:

- **Alarms are never held.** Anything that isn't `*.measurement`/`*.change` is sent to the cloud
  immediately by the CGI itself, as its own single-item batch. Only if that send fails is it
  spooled for the next drain — never both, so an alarm is never double-reported.
- Devices whose newest values are UNCHANGED since the last successful report ride the `ok` list
  ("all my devices are good, except these") — a freshness touch, not data.
- Every `KEYFRAME_EVERY`th drain is a full keyframe, bounding how long a lost delta can
  leave the cloud's view stale.
- A failed drain retries with the SAME sequence number, so the cloud can drop a replay whole
  instead of re-firing its alerts.

Pure shell throughout — Lua is not stock GL.iNet firmware. ⏳ Before shipping to customers:
confirm `uhttpd` is present on a FACTORY-RESET X750 (it was present on the bench unit, which has
had packages added); if not stock, it becomes a `Depends:` in the .ipk.


## The LAN management door (hub-lite 0.14.0)

The same uhttpd instance also serves **`hub-lite-mgmt.sh` at
`http://<router>:8722/cgi-bin/mgmt`** — the door an app uses to manage this hub-lite while it is
aboard, instead of driving the ROUTER VENDOR's API or waiting out the cloud command queue.

Owner, 2026-08-21. The app had two ways to reach a router and neither of them was this one. It
dialled the vendor API at the router's LAN address — which works only aboard, only on GL.iNet, and
tells you nothing about the hub-lite — or it queued a verb in the cloud and waited for the next
check-in. Aboard, on the same LAN as the box, the honest thing is to ask the hub-lite and get an
answer now.

| Call | What it does |
| --- | --- |
| `GET  ?action=status` | The last telemetry this hub-lite composed, verbatim from its state file |
| `GET  ?action=lockdown` | `uci show firewall` for our rules, in the text the app's parser expects |
| `POST ?action=command&cmd=<verb>` | ONE allowlisted verb, run now — the cloud verb set exactly |
| `POST ?action=lockdown&catch=0\|1` | Apply, with the per-MAC allow list the cloud verb cannot carry |

The door is **not part of the relay tier**: `HUB_LITE_ENABLED` turns the webhook receiver on, and
since 0.14.1 the uhttpd instance and the management-key fetch run regardless of it. Tying the
listener to that flag meant a hub-lite that merely reported telemetry had no door at all, so the app
fell back to driving the router VENDOR's API — the thing the door exists to replace. Both CGIs are
independently deny-by-default, so serving them costs a listener and nothing else. What the door
still needs is `uhttpd` present (**not stock** on GL.iNet firmware — the `.ipk` carries it as a
`Depends:`).

**Auth** is the router's own management key (`MGMT_KEY` in the config), presented as `x-brvg-key`.
The hub-lite fetches it from `/api/agent/mgmt-key` with the device token it already has, so a box
enrolled before 0.14.0 picks its key up on the next modem tick with nothing to re-install. With no
key on the box the door is **shut**, not open: a hub-lite that has never reached the worker cannot
tell a member from a stranger on the marina Wi-Fi.

⚠️ **A hub-lite does NOT get the vehicle's per-member key set** the way a full hub does
(`/api/hub/keys` refuses a router's token on purpose). That set is every member's LAN management
access and belongs on a host that can resolve roles; this is a router in a locker. One key, one
router, one privilege level — and the worker gates issuing it at the same boundary as
`/api/agent/command`.

**Not a tunnel**, for the same reason the cloud queue is not one: `command` takes a verb off the
allowlist `run_commands` already owns (so the two doors can never diverge about what a hub-lite
will do), and `lockdown` takes a boolean and hardware addresses that are validated before they
reach a `uci` argument. This REPLACES the app SSH-ing a generated `uci` script in as root, which is
why `valid_mac` and the body filter are the interesting part of the file rather than an
afterthought.

**Status is served from a file, never re-collected.** `write_state` in the reporting path writes
what it just told the cloud to `/tmp/brvg-hub-lite.state`, and the CGI cats it. Re-collecting in
the CGI would contend with the main loop for the AT port, spend modem time on every page view, and
let the two paths disagree about the same instant.


## Hub watchdog — failing open, except in bandwidth saver mode

Owner decisions 2026-08-13 + 2026-08-17. **`BANDWIDTH_SAVER=1` disables the watchdog entirely and
lockdown FAILS CLOSED**: when lockdown exists to control metered-SIM spend, a dead hub must not
release it — a silent vessel until the connectivity-offline alert fires is the accepted trade.
Everything below applies only to normal (non-saver) installs.

Under a *traffic* lockdown the hub is the only route to the cloud. If
the hub runs on this router that is fine — a dead router is a dead gateway either way — but a hub
on a **separate** box (Pi, Docker, desktop) can die while the router routes happily, and then the
vessel is silent with no remote way back in.

Set `HUB_WATCH_URL` to the hub's `/healthz` and the router will watch it. After `HUB_WATCH_FAILS`
consecutive failures it **removes the lockdown rules** so sensors can reach the cloud directly
again, and sends `hub.offline` so you hear about it. When the hub returns it sends `hub.online`.

It deliberately does **not** re-arm the lockdown on recovery: a hub that restarts every few minutes
would flap the firewall, and every apply is a ~10 s reload. The released state is announced;
re-applying is one tap in the app.

The `brvg_lk_` rule-name prefix is the shared contract between this watchdog and the app's SSH
enforcement — both manage the same rules by matching that prefix, which is how two languages stay
in step without sharing code.


## WAN usage accounting

Every modem report also carries per-source usage: `wanSrc` (which WAN is carrying traffic right
now) and `wanKb_cellular` / `wanKb_wifi` / `wanKb_wired` deltas. No configuration — it reads
`/sys/class/net/*/statistics/*_bytes` and degrades silently where those don't exist (macOS, a
container without them).

**Why deltas and not totals.** Those counters are since-boot, and three separate things send them
backwards: a reboot, an interface bounce, and our own `reset_data` command. `wan_delta` treats any
DECREASE as a reset and reports the new value — otherwise unsigned arithmetic produces a
wrap-sized spike that the cloud would read as a runaway plan burn.

The modem's own `AT+QGDCNT` counter still drives the plan alerts, because that is what the carrier
meters. These interface counters answer the different question — *where* the bytes went — which is
what makes the Wi-Fi uplink and the roll-up measurable rather than merely plausible. Cross-checked
on the bench: QGDCNT and `wwan0` agree to ~1% over 14 h.

## Hub watchdog and hub health

`HUB_WATCH_URL` should point at the hub's `/healthz`. That endpoint reports **delivery**, not just
liveness: it answers 503 once the hub has failed several consecutive drains, so a hub that is
running but cannot reach the cloud still trips the watchdog and releases the lockdown. A hub whose
web server is up while nothing is being delivered is precisely the case fail-open exists for.
