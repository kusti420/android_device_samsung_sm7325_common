# abyssd v1-lite (r2)

The daemon behind Abyss, the keys-only, low-power music mode of IchthysOS (Galaxy A52s 5G, a52sxq). It is written in Rust. When Android's framework is stopped, abyssd takes over the screen (its own DRM renderer and a baked bitmap font), the three side keys and the 3.5 mm jack.

Installed as `/system_ext/bin/abyssd` (`rust_binary` in `../Android.bp`). The service and the enter/leave actions are in `../abyss.rc`, installed through `init_rc`.

## What v1-lite does (r2, after the first on-device run)

- **Screen.** The canvas boards (Main, Library, Player, Help) redrawn for the 1080 x 2400 panel, in a KMS dumb buffer on `/dev/dri/card0`, using the 60 Hz panel mode (the 120 Hz mode is skipped). OLED palette: black, text #A8A290, dim #7D7869, accent #5DB04C, warn #D08A2A, lines #25231C. The selected row is "▸ " + bold accent; values sit dim on the right of each row. **Every screen ends with a key guide** for what the keys do on that screen. Fonts: SMALL 40 px (status, guide), BODY 48 px and BODY_BOLD 48 px (rows), BIG 72 px (titles, numbers). A frame is pushed only on change; the screen turns off `persist.abyss.screen_timeout_s` after the last key (default 15 s, 5..120 s, read at start; the ● ● peek uses the same time; Help: 30 s or the setting, whichever is longer).
- **Brightness.** abyssd cannot write the panel's sysfs node (vendor type), so after the first frame and on every wake it sets `sys.abyss.brightness` to `persist.abyss.brightness` (default 160, clamped to 1..486) and abyss.rc writes that to `panel0-backlight` while `sys.abyss.state=active`. init saves Android's level on enter and restores it on every leave path. Screen off is DPMS off.
- **Screens.**
  - Home: "ICHTHYS ABYSS", "Android is asleep…", status lines (battery % and ≈ hours of music, output, radios off), then Music ›, Now playing ("Continue" when paused or only remembered: plays and opens it, library or radio), the installed modules, Bluetooth › (off / on / the connected headphones), Battery, Help, Back to Android · about 30 s, Power off (separate "Yes, power off" row). Library lists open where you left them (per screen, this session); an artist with one album opens the album; a letter with one name opens it. Music › Output › opens Bluetooth.
  - Music: Resume (song · position), Shuffle all, Artists ›, Albums ›, Folders › (A–Z; lists longer than 24 open on a letter grid first: # A … Z), AbyssTest, Output (auto · USB / 3.5 mm), Back. Artist → Play all + albums; Album → Play all + numbered tracks; Folder → Play all + subfolders + tracks. Every list has Back first and last.
  - Now playing: state word + volume, title, artist · album (tags), track n of m, elapsed/total bar, "music left on this battery" box, the output in use, a volume bar, up next. With nothing plugged in it says **"No headphones. Plug in wired or USB headphones, or connect Bluetooth headphones: menu › Bluetooth."** and playback stays paused. On Bluetooth the output line reads "Bluetooth · <name>" and the format "… · CPU decode".
  - Bluetooth: status line ("Turning on…", "Connecting to <name>…", "Connected · <name>", "Couldn't connect: <reason>", "Bluetooth stopped."), then "Connect <last headphones>" first (also when off: it turns Bluetooth on, then connects), "Turn on", the paired list from the helper, "Disconnect", "Turn off", "Refresh list". Key guide like the other menus.
  - Battery, Help (where you are + the full key map).
- **Keys** (`src/keys.rs`, DECISIONS "Added 18:58"):

  | Where | Keys | Action |
  |---|---|---|
  | Now playing, and screen off with music loaded | tap ▲ / tap ▼ | volume (screen on: a volume box for 2 s; screen off: stays dark) |
  | same | hold ▲ ≥ 600 ms / hold ▼ ≥ 600 ms | next / previous |
  | same | ● | play / pause (after the 400 ms double-press window) |
  | same | ● ● | the menu; with the screen off a "look": wakes on Now playing (`persist.abyss.nav.wake=last`: on the screen you left), ● ● again returns |
  | same | ▲ + ▼ (short) | play / pause |
  | Menus | ▲ ▼ / ● | move / select (on release); lists wrap around |
  | Menus | hold ▲ / hold ▼ | keeps moving (after 600 ms, a row every 150 ms, stops after 5 s) |
  | Menus (library and module screens) | ▲ + ▼ (short) | Back one level (the cursor is not moved first); nothing on Home |
  | Screen off, nothing loaded or nothing plugged in | first press | only wakes the screen |
  | Anywhere | hold ▲ + ▼ 2 s | Help |
  | Anywhere | hold ● 3 s | "Back to Android in 3 s, any key cancels" |
  | Anywhere | key held > 5 s | ignored until released (the ● hold is the exception) |
  | Anywhere | ▼ + ● short (both down within 300 ms, first release within 1.5 s) | **Capture** (see below); fires on release |
  | Anywhere | ▼ + ● held longer | never acted on (Samsung force restart = ~7 s hold) |
  | Headset remote | 1× / 2× / 3× | play·pause / next / previous (presses within 450 ms count together); its own volume keys = volume |

  Starting any song (Resume, Shuffle, an album, a track…) opens Now playing.
- **Capture** (`src/capture.rs`, DECISIONS "Capture chord"). ▼ + ● short = the screenshot chord. What it captures depends on the screen: each kind is a `CaptureSource` (`fn capture(&self) -> Option<Capture>`; `Capture` = kind, one-line `line`, `fields` map, timestamp), `capture::sources()` lists the ones for the current scene, the first that returns something wins. Today there is one source, **music**: on Now playing, with the screen off while a song is loaded (playing or paused), and as the fallback on every other screen while a song is loaded. It saves `Artist - Title (Mix) (LABEL CATNO)` from the file's tags (`tags::read_raw`): ARTIST/TPE1 (several ARTIST values joined with ", "; album artist as fallback), TITLE/TIT2 (file name as fallback); the mix comes from the title's last `( )`/`[ ]`, else VERSION / MIX / SUBTITLE / TIT3 / TXXX:MIX / TXXX:VERSION, else a remixer (MIXARTIST / REMIXER / TPE4 → "<name> Remix"); label = LABEL / ORGANIZATION / PUBLISHER / TPUB / TXXX:LABEL; catalogue number = CATALOGNUMBER / LABELNO / CATALOG NUMBER / CATNO / TXXX:CATALOGNUMBER. Missing parts are left out (no empty parentheses). Each capture is one JSON line appended to `/data/misc/abyss/out/captures.jsonl` (O_APPEND, one write, fsync; mode 0640, group system through the setgid dir), e.g. `{"fields":{"abyss_screen":"NowPlaying","album":"…","artist":"…","catalog_number":"…","codec":"FLAC","duration_ms":…,"label":"…","mix":"…","path":"/data/media/0/Music/…","playing":true,"position_ms":…,"screen_on":true,"tags":{"ARTIST":"…",…},"title":"…"},"kind":"music","line":"Artist - Title (Mix) (Label CAT01)","time":"2026-11-07T22:59:01Z","time_ms":…,"v":1}`. `tags` holds every text field of the file (Vorbis comment names / ID3 frame ids, TXXX as `TXXX:<DESC>`; a repeated key becomes an array; pictures and values over 4 KB left out). Feedback: screen on = "Saved: <line>" for 2 s; screen off = one 40 ms buzz on `/sys/class/timed_output/vibrator/enable` (best effort, the screen stays dark; it needs `allow abyssd sysfs_vibrator:file w_file_perms;`, not in the policy yet, and the node writable for uid root / group system without CAP_DAC_OVERRIDE; until then it logs `capture: no haptic` once); nothing to capture or a write error = the screen wakes with the reason. In menus a ▼-first chord also moves the selection one row (▼ moves on press); ●-first does not.
- **Output** (`src/audio.rs`): Bluetooth headphones (while connected) > USB DAC > 3.5 mm jack > nothing. Losing Bluetooth pauses, even with a wire plugged in (the wire is then selected, paused); the same headphones back within 5 min play on. USB cards come from `/proc/asound/cards` (driver `USB-Audio` with a `pcm0p`), re-read every 2 s while the screen is on or a USB DAC plays, else every 10 s; a pulled DAC also fails the next write at once. USB: tinyalsa `pcm_open(card, 0, PCM_OUT)`, hw params queried (rate range, formats): the file's rate, then 48 / 44.1 / 96 kHz, S16_LE else S32_LE / S24_3LE / S24_LE (samples widened). Jack: `deep-buffer-playback headphones` (RX_CDC_DMA_RX_0 Audio Mixer MultiMedia1) + `headset` (ear: WCD938x HPH, LOHIFI, HPH digital volume 73) from `/vendor/etc/mixer_paths.xml`, PCM hw:0,0 (MultiMedia1). Volume is software gain on a 40 dB scale for both, ceiling 70 %.
- **DSP decode on the jack** (`CompressOut` in `src/audio.rs`): FLAC (16/24 bit, <= 48 kHz, 1-2 ch) and MP3 are not decoded by abyssd at all. symphonia only demuxes; the encoded frames go to the Qualcomm ADSP through the compress-offload device `/dev/snd/comprC0D7` (Compress1 = MultiMedia4; `USECASE_AUDIO_PLAYBACK_OFFLOAD` id 7), paths `compress-offload-playback headphones` (RX_CDC_DMA_RX_0 Audio Mixer MultiMedia4) + `headset`. Fragments: 1 MB x 2, else 256 KB x 4, else 32 KB x 4, so the AP wakes about once per 1 MB (about 9 s of FLAC 16/44.1, about 26 s of MP3 320). Volume = the stream's `Compress Playback 7 Volume` control (Q13 linear, 0x2000 = 0 dB), same 200-step 72 dB curve; without that control the track falls back to CPU decode. While the DSP plays and the player thread only waits for it, abyssd may suspend the AP (the DSP's write-done interrupt wakes it). Pause keeps the stream for 30 s, then closes it (resume reopens at the position). End of track = drain (no gapless yet). Everything else (USB DAC, other codecs, > 48 kHz, an offload error) = CPU decode; Now playing shows "DSP" or "CPU decode" next to the format. Raw compress ioctls, not libtinycompress (that library is vendor-only; abyssd is system_ext). Log lines: `offload: flac 44100/16 2ch fragment 1048576x2 on /dev/snd/comprC0D7`, `offload: not used (...): cpu decode`. Unplug (jack or USB) pauses at once; music never moves to the speaker; the same output back within 5 min plays on (a USB DAC is matched by name). "Play" with nothing plugged in stays paused; plugging in within 5 min then plays.
- **Music.** `.flac` and `.mp3` under `/data/media/0/Music` and `/mnt/media_rw/*`. Tags (title, artist, album artist, album, track number) are read by `src/tags.rs` (FLAC Vorbis comments, ID3v2.3/2.4) and cached in `/data/misc/abyss/out/tags.tsv` (path + size + mtime), so only new files are opened after the first entry. Decoding: symphonia (FLAC, MP3) on its own thread. Resume state: `/data/misc/abyss/out/state.json` (tmp + fsync + rename).
- **Bluetooth** (`src/bt.rs`, `BtOut` in `src/audio.rs`; protocol: abyss-design reports/abyss-bt-protocol.md). Off by default every session. "Turn on" sets `sys.abyss.bt=on`; abyss.rc then starts `abyss-btd` (Android's Bluetooth stack without the framework, `/apex/com.android.bt/bin/abyss-btd`). abyssd connects to `/dev/socket/abyss_bt` (SOCK_SEQPACKET; byte 0 = 0x00 JSON, 0x01 PCM), asks for the paired list and connects the picked headphones. Playback over Bluetooth is CPU decode: s16le stereo at the rate the A2DP codec wants (44.1/48 kHz, resampled), 20 ms packets, paced by the helper's backpressure; volume = the same 200-step software curve, the headset is kept at full scale (AVRCP absolute volume 127). Pause = `pause` (stream suspended). AVRCP keys from the headphones = the headset button's actions (play/pause/next/prev/volume). No suspend while Bluetooth plays or is turning on/off/connecting. "Turn off" sends `off` and then `sys.abyss.bt=off`; leaving Abyss stops the helper before the framework starts. The last headphones are remembered in `/data/misc/abyss/out/bt_last.json`. Abyss Light is retired: abyssd never writes `persist.abyss.after_leave` any more.
- **Audio HAL settle / restarts**, **battery** (30 s reads, ≈ hours = capacity × 4500 mAh × 0.97 ÷ average mA, 3 % reserve power-off), **leaving** (exit 0 after `sys.abyss.request=leave`, SIGTERM exit well inside 200 ms, crash guard, never writes `sys.abyss.state`) and **logs** (logcat `abyssd` + `/data/misc/abyss/out/session.log`) are unchanged from r1.

- **Wi-Fi leases** (`src/wifi/`, report: abyss-design reports/impl-wifi.md). Off unless something holds a lease (`Wifi::acquire(holder) -> Lease`; drop or `release(next_use_in_ms)`; the last release lingers `persist.abyss.wifi.linger_s`, default 30 s, or until a hinted next use within `persist.abyss.wifi.linger_max_s`, default 600 s, then goes off). Bring-up: `sys.abyss.wifi=on` → abyss.rc starts Android's `wpa_supplicant` and `abyss-net`; abyssd adds wlan0 over `/dev/socket/wpa_wlan0` with the networks from `/data/misc/abyss/handoff/wifi.json` (written by the entry app before airplane mode; one-channel attempt on the last BSSID first, then a full scan), runs its own DHCP client (AF_PACKET; INIT-REBOOT with the cached address in `out/wifi_lease.json`, else DISCOVER with Rapid Commit; renew at T1), and asks `abyss-net` (`abyssd --net-helper`, uid system: netd hangs for any other uid while system_server is stopped) to configure netd (network 42, address, routes, default). DNS: abyssd's own stub resolver against the DHCP servers. Home shows `wi-fi off` / `on (<ssid>)`; Battery has a **Wi-Fi test** row (lease → GET `persist.abyss.wifi.test_url`, default http://connectivitycheck.gstatic.com/generate_204 → release). `persist.abyss.wifi.mac=factory` uses the factory MAC instead of Android's per-network one. No suspend while Wi-Fi is coming up or a test runs.

## What v1-lite does not do (yet)

- Gapless DSP playback (next_track / partial drain), AAC (no AAC demuxer is vendored), DSP decode on the USB DAC.
- Speaker output, chords (▲ ▲ ●, …), voice, alarms, scripts, the idle "fish" look (Idle board). Wi-Fi for other uids (scripts) needs IDnsResolver configuration (not done).
- The Estonian order (Õ Ä Ö Ü after W): letters fold accents into A–Z.
- uevent-driven USB detection (polling instead; no netlink socket needed).
- The energy-model hours estimator, panel low-power peeks, CPU caps, turning off touch gestures.
- Deep sleep (`suspend.rs`, off by default, `persist.abyss.suspend=1`).

## Properties

| Property | Default | Meaning |
|---|---|---|
| `sys.abyss.request` | – | abyssd writes `leave` |
| `sys.abyss.wifi` | `off` | abyssd writes `on`/`off` (first lease / last release); abyss.rc starts/stops wpa_supplicant + abyss-net, resets it to `off` on enter and leave |
| `persist.abyss.wifi.linger_s` / `.linger_max_s` / `.mac` / `.test_url` | `30` / `600` / `handoff` / generate_204 | Wi-Fi lease tuning (read at start) |
| `sys.abyss.bt` | `off` | abyssd writes `on`/`off` (Bluetooth switch); abyss.rc starts/stops abyss-btd, and resets it to `off` on enter and leave |
| `persist.abyss.brightness` | `160` | backlight level in Abyss (1..486); abyssd only reads it |
| `persist.abyss.screen_timeout_s` | `15` | screen-off time after the last key, in seconds (clamped to 5..120; read at start) |
| `persist.abyss.nav.start` | `player` | `player`: Abyss opens on Now playing with the last song loaded paused at its place (one ● plays it, screen on or off); `home`: opens on Home (read at start; Settings › Abyss) |
| `persist.abyss.nav.wake` | `player` | what ● ● with the screen off shows while music is loaded: `player` = Now playing, `last` = the screen you left (read at start; Settings › Abyss) |
| `sys.abyss.brightness` | – | abyssd writes the level; abyss.rc applies it to the panel |
| `sys.abyss.hal` | – | watched; `restarted` makes abyssd re-apply the audio route |
| `persist.abyss.suspend` | `0` | experimental suspend loop |
| `persist.abyss.audio.card` | `0` | jack ALSA card |
| `persist.abyss.audio.device` | `0` | jack PCM device (0 = deep buffer, 9 = low latency) |
| `persist.abyss.audio.mixer_xml` | `/vendor/etc/mixer_paths.xml` | mixer paths file |
| `persist.abyss.audio.paths` | `deep-buffer-playback headphones,headset` | comma-separated paths to apply |
| `persist.abyss.audio.offload` | `true` | DSP decode on the jack (false = always CPU decode) |
| `persist.abyss.audio.offload_device` | `7` | compress device (comprC0D7) |
| `persist.abyss.audio.offload_paths` | `compress-offload-playback headphones,headset` | paths for DSP decode |
| `persist.abyss.audio.offload_frags` | `1048576x2,262144x4,32768x4` | fragment plans, tried in order |

## Running it by hand (root adb shell)

```sh
adb root && adb shell
stop zygote_secondary; stop zygote          # framework off
stop audioserver; stop surfaceflinger
stop vendor.qti.hardware.display.composer   # releases DRM master
sleep 3                                     # let the audio HAL restart settle
/system_ext/bin/abyssd                      # runs in the foreground; logs: logcat -s abyssd
# Ctrl-C (SIGINT) stops it cleanly; then bring Android back:
start vendor.qti.hardware.display.composer; start surfaceflinger
start audioserver; start zygote; start zygote_secondary
```

The normal way in is the rc: `setprop sys.abyss.request enter` from a booted, unlocked phone.

## Source layout

| File | Role |
|---|---|
| `src/main.rs` | event loop (poll on evdev fds, the player notify socket, the HAL watch socket and a signal pipe; the poll timeout is the timer), logging |
| `src/display.rs` | KMS: connector/mode/CRTC selection, dumb buffer, flip/SETCRTC, DPMS blank |
| `src/font.rs`, `src/font_data.rs` | baked 1-bit font and renderer |
| `src/input.rs` | evdev keys and the jack switch (CLOCK_BOOTTIME timestamps) |
| `src/keys.rs` | key grammar state machine (pure, with unit tests) |
| `src/capture.rs` | capture harness (`CaptureSource`, `captures.jsonl`) and the music source |
| `src/ui.rs` | screens and key guides |
| `src/library.rs` | scan, sort, albums/artists, letter row, resume state |
| `src/tags.rs` | FLAC / ID3v2 tag reader (library fields + every raw text field) + tags.tsv cache |
| `src/backlight.rs` | asks init for the backlight level (`sys.abyss.brightness`) |
| `src/player.rs` | decode thread |
| `src/audio.rs` | `AudioOut`: tinyalsa jack and USB DAC backends, output detection, `NullOut`, HAL placeholder |
| `src/battery.rs`, `src/control.rs`, `src/suspend.rs`, `src/clock.rs` | battery, properties/leave/power off, experimental suspend, boot clock |

`unsafe` appears only in `audio.rs` (FFI) and `input.rs` (one ioctl). Each block has a SAFETY comment.

## Font

`src/font_data.rs` is generated by `tools/gen_font.py` (needs Pillow) and committed:

```sh
python3 tools/gen_font.py --small 40 --body 48 --big 72 > src/font_data.rs
```

- **Source:** DejaVu Sans Mono 2.37 (+ Bold), from the Arch `ttf-dejavu` package (`/usr/share/fonts/TTF/DejaVuSansMono.ttf`). DejaVu Sans 2.37 is the fallback for any symbol the mono face lacks.
- **License:** Bitstream Vera Fonts license; DejaVu's changes are in the public domain. Both are permissive and allow embedding and redistributing derived bitmaps. The full text is at https://dejavu-fonts.github.io/License.html.
- **Faces:** SMALL 40 px, BODY 48 px, BODY_BOLD 48 px (DejaVu Sans Mono Bold), BIG 72 px.
- **Coverage (all faces):** ASCII, Latin-1, Latin Extended-A, and `▸▲▼●✓≈·–—…→←›‹“”‘’€°×−`.
- **Missing characters** are drawn as a dim box.
