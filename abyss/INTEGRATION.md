# Abyss v1-lite: platform integration

How Android hands the phone to `abyssd` and takes it back. Three pieces:

| Piece | Path | Installs as |
|---|---|---|
| init script | `abyss/abyss.rc` | `/system_ext/etc/init/abyss.rc` (via `init_rc` on the `abyssd` rust_binary) |
| SELinux policy | `abyss/sepolicy/private/` | product policy (same place as `sepolicy/private/ichthys_*`) |
| Entry app | `packages/apps/AbyssEntry` | `/system_ext/priv-app/AbyssEntry` (`org.ichthys.abyss`, system uid) |

`/system_ext` is folded into `/system` on this device (`/system/system_ext`), which is why
`file_contexts` lists both spellings.

## Build wiring (not done yet, needs .mk / Android.bp edits)

- `abyss/Android.bp`, on the `abyssd` rust_binary: `init_rc: ["abyss.rc"]` (plus
  `system_ext_specific: true`). Soong installs it to `/system_ext/etc/init/` (every module
  type has `init_rc`, build/soong/android/module.go:576).
- `BoardConfigCommon.mk`, after `PRODUCT_PUBLIC_SEPOLICY_DIRS += $(COMMON_PATH)/sepolicy/public`:
  `PRODUCT_PRIVATE_SEPOLICY_DIRS += $(COMMON_PATH)/abyss/sepolicy/private`
- `common.mk`: `PRODUCT_PACKAGES += abyssd AbyssEntry`

## Property protocol

| Property | Values | Written by | Read by |
|---|---|---|---|
| `sys.abyss.request` | `enter`, `leave`, `""` | entry app (`enter`), abyssd (`leave`), abyss.rc (crash guard `leave`, reset `""`) | init |
| `sys.abyss.state` | `off`, `entering`, `active`, `leaving` | **init only** (abyss.rc) | abyssd, entry app |
| `sys.abyss.hal` | `restarted` (written again each time) | **init only** | abyssd (watches the serial) |
| `persist.abyss.*` | settings, e.g. `persist.abyss.suspend`, `persist.abyss.audio.card` | Settings / entry app (system_app), root shell | abyssd |

SELinux types (`sepolicy/private/property.te`): `abyss_prop` (the `sys.abyss.` prefix and
`sys.abyss.request`), `abyss_state_prop` (`state`, `hal`), `abyss_config_prop` (`persist.abyss.`).
`sys.abyss.request` and `sys.abyss.state` are typed enums; `""` is always allowed (clears).

```
         request=enter (state=off, boot_completed=1)
  off ──────────────────────────────────────────────▶ entering ──(start abyssd)──▶ active
   ▲                                                                                 │
   │                    request=leave  (abyssd: power held 3 s)                      │
   └───────────── leaving ◀──────────── or crash guard: init.svc.abyssd=stopped ◀────┘
```

Init property triggers fire on every write, also when the value does not change, so a request
that was dropped (wrong state) can simply be written again.

### Enter (`sys.abyss.request=enter`)

1. `state=entering`
2. `stop` in this order: `zygote_secondary zygote cameraserver media.swcodec mediaextractor
   mediametrics media media.transcoding audioserver surfaceflinger
   vendor.qti.hardware.display.composer`
3. `start abyssd`
4. `state=active` (if abyssd already died, this write fires the crash guard)

### Leave (`sys.abyss.request=leave`, state `active`)

1. `state=leaving` (so abyssd's own "stopped" does not fire the crash guard)
2. `stop abyssd` (SIGTERM, SIGKILL after 200 ms: `gentle_kill`)
3. `start vendor.qti.hardware.display.composer surfaceflinger audioserver media mediaextractor
   mediametrics media.swcodec cameraserver zygote zygote_secondary`
4. `state=off`, `request=""`

### Guards

- Crash guard: `init.svc.abyssd=stopped` while `state=active` → `request=leave`.
- Audio guard: `init.svc.audioserver=running` while `state=active` → `stop audioserver`,
  `sys.abyss.hal=restarted` (see below).

### What keeps running

netd, vold, logd, servicemanager, hwservicemanager, vndservicemanager, system_suspend, keystore2,
ril-daemon, cnss-daemon, adbd (USB adb keeps working), all vendor HALs incl. vendor.audio-hal.
Nothing is stopped by class.

## Contract for abyssd

1. **Display.** SurfaceFlinger and the composer are killed asynchronously. Open
   `/dev/dri/card0`; the first opener after the composer exits becomes DRM master on its own
   (drm_auth.c `drm_master_open`). If a modeset fails with EACCES/EBUSY, close and reopen every
   100 ms for up to 5 s, then exit non-zero (the crash guard brings Android back).
   `DRM_IOCTL_SET_MASTER` needs CAP_SYS_ADMIN on 5.4 (DRM_ROOT_ONLY) and is not granted: treat
   its failure as a warning only. abyssd cannot read `init.svc.vendor.*` (vendor_internal_prop,
   coredomain neverallow), so it cannot see the composer's state; `init.svc.surfaceflinger` is
   readable.
2. **Audio settle.** `stop audioserver` makes audioserver.rc stop and start `vendor.audio-hal`.
   That restart runs *after* the enter action, i.e. after abyssd has started, and the fresh HAL
   resets the mixer while it initialises. abyssd must wait until `init.svc.audioserver=stopped`
   and then ≥ 3 s (the going-dark screen covers it) before applying its mixer path and opening the
   PCM. Before each `pcm_open`, apply the path again.
3. **HAL came back.** Each time init writes `sys.abyss.hal=restarted`, the HAL is being restarted
   once more: close the PCM, wait 3 s, re-apply the path, reopen. More than 3 in 60 s = give up
   and write `sys.abyss.request=leave`.
4. **Leaving.** Write `sys.abyss.request=leave` (after the 3 s power-key hold, or on a fatal
   error). Never write `sys.abyss.state`. On SIGTERM: restore touch/panel state, close DRM and
   PCM, exit within 200 ms.
5. **Power off.** `sys.powerctl=shutdown` (init does a clean shutdown).
6. **Files.** `/data/misc/abyss/{conf,handoff}` read, `/data/misc/abyss/out` write with
   tmp + rename. Dirs are `02770 system:system`; abyssd runs as uid root with group system, so set
   `umask(0o007)` and create files `0660` (they inherit group system from the setgid dir).
7. **Music.** Read only, below FUSE: `/data/media/0/...` and `/mnt/media_rw/<uuid>/...`. No
   DAC override: access comes from the `media_rw` group (`/mnt/media_rw/<uuid>` is root:media_rw
   mask 0007, PublicVolume.cpp:151-158).
8. **Suspend (experimental).** `/sys/power/wakeup_count` read, write it back, then `mem` to
   `/sys/power/state`. abyss.rc makes `wakeup_count` 0660 system on boot.

## Testing by hand (root adb shell, KernelSU, `u:r:ksu:s0`)

Use USB adb: Wi-Fi adb dies with the framework. All commands run in `adb shell` after `su`.

### A. The real path (rc + policy)

```sh
getprop sys.abyss.state                 # off   (empty = abyss.rc not installed)
setprop sys.abyss.request enter
sleep 3; getprop sys.abyss.state        # active
pidof system_server surfaceflinger audioserver   # expect nothing  [verify: system_server]
getprop init.svc.abyssd                 # running
ps -AZ | grep abyssd                    # u:r:abyssd:s0 ... /system_ext/bin/abyssd
dmesg | grep 'avc: denied' | grep abyssd   # policy gaps
setprop sys.abyss.request leave         # or hold power 3 s
sleep 2; getprop sys.abyss.state        # off; Android boots back (about 30 s)
```

Crash guard: while active, `kill -9 $(pidof abyssd)` → Android must come back on its own.

### B. The same steps by hand (no rc triggers involved)

Leave `sys.abyss.request` alone: with `state=off` none of the rc triggers fire.

```sh
# enter, same order as abyss.rc
stop zygote_secondary
stop zygote
stop cameraserver
stop media.swcodec
stop mediaextractor
stop mediametrics
stop media
stop media.transcoding
stop audioserver            # audioserver.rc restarts vendor.audio-hal by itself
stop surfaceflinger
stop vendor.qti.hardware.display.composer
getprop init.svc.audioserver; getprop init.svc.surfaceflinger   # stopped stopped
getprop init.svc.vendor.audio-hal                               # running (fresh pid)

# run abyssd in its real domain, under init (seclabel u:r:abyssd:s0):
start abyssd
logcat -s abyssd            # its log
stop abyssd

# or in the shell's own domain (no SELinux check of abyssd's policy; output on the terminal):
/system_ext/bin/abyssd      # Ctrl-C to end

# leave, same order as abyss.rc
start vendor.qti.hardware.display.composer
start surfaceflinger
start audioserver
start media
start mediaextractor
start mediametrics
start media.swcodec
start cameraserver
start zygote
start zygote_secondary
```

`stop` / `start` with no arguments (AOSP toolbox) is NOT equivalent: it also stops netd and
leaves the composer, cameraserver and media services running, so abyssd would not get DRM master.

## Not covered by v1-lite policy (later)

- **Deep sleep.** `ISuspendControlService.enableAutosuspend` is the real fix; finding
  `system_suspend_control_service` is a platform neverallow (system_suspend.te:59-67). Needs a
  system/sepolicy exception.
- **Backlight / touch / Wi-Fi nodes.** `/sys/class/backlight/panel0-backlight` is
  vendor_sysfs_graphics, `/sys/class/sec/tsp` sysfs_sec_touchscreen, `/dev/wlan` a vendor type.
  Vendor policy cannot name the product-private `abyssd` type; this needs an attribute in
  product public policy (like `ichthys_udfps_prop` in sepolicy/public) plus vendor rules.
- **CPU hotplug / schedutil cap** (cpu4-7 offline): sysfs_devices_system_cpu writes, not granted.
- **/dev/socket/abyss** (CLI, v1.1), **INetd/DnsResolver** (Wi-Fi, v1.1, platform neverallow
  netd.te:173-190), **abyss-guard** (separate power-key service).
