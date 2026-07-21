#!/vendor/bin/sh
#
# IchthysOS/AOSP-17 a52sxq bringup: crash-log-to-kmsg bridge.
#
# Stream logcat's low-volume `crash` buffer into /dev/kmsg. The crash buffer only
# receives Java FATAL EXCEPTION traces and native debuggerd/abort summaries, so it
# never floods -- unlike the main buffer, whose PackageManager feature-flag spam was
# evicting the system_server tombstone out of the 256KB pmsg ring before we could read
# it. /dev/kmsg lands in Samsung's 2MB sec_log, readable as /proc/last_kmsg from
# recovery after a warm reset, which survives a short boot intact.
#
# NOTE: use shell redirection, not `logcat -f`. logcat's -f rotates its output file
# and that rotation (lseek/ftruncate) fails on the /dev/kmsg character device, so the
# earlier `logcat -b crash -f /dev/kmsg` service exited status 1 without logging a byte.
#
# TODO(bringup): remove once the device boots and live adb logcat is available.

# Full path: the minimal vendor /vendor/bin/sh PATH does not include /system/bin,
# so a bare `logcat` fails with "inaccessible or not found" (exit 127).
exec /system/bin/logcat -b crash -v threadtime >> /dev/kmsg 2>&1
