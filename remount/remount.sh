#!/system/bin/sh
#
# Copyright (C) 2026 IchthysOS
# SPDX-License-Identifier: Apache-2.0
#
# `adb remount` for IchthysOS: make system, vendor, product and odm writable in place.
#
# AOSP's remount refuses to run here (it wants ro.debuggable=1 and an "orange" boot state), and
# for dynamic partitions it always goes through overlayfs. With no scratch partition the
# overlay ends up on /cache, which first-stage init never mounts, so everything pushed was gone
# after a reboot (kusti420/ichthysos-tracker#51).
#
# The images are built with reserve space instead (BOARD_*IMAGE_PARTITION_RESERVED_SIZE), and
# none of them is under dm-verity, so the partitions are simply remounted read-write and pushed
# files are written into the real filesystem. They are read-only again after the next boot; a
# full OTA replaces whatever was pushed.

usage() {
    echo "usage: remount [-h] [-R] [partition]..."
    echo "  Remount system, vendor, product and odm (or the given partitions) read-write."
    echo "  -R  reboot afterwards"
}

# Prints "device fstype options" of the topmost mount at $1.
find_mount() {
    local dev mnt fstype opts rest found=
    while read -r dev mnt fstype opts rest; do
        [ "$mnt" = "$1" ] && found="$dev $fstype $opts"
    done < /proc/mounts
    echo "$found"
}

reboot_after=false
while getopts hR opt; do
    case "$opt" in
        h) usage; exit 0 ;;
        R) reboot_after=true ;;
        *) usage >&2; exit 1 ;;
    esac
done
shift $((OPTIND - 1))

if [ "$(id -u)" != 0 ]; then
    echo "remount: not running as root" >&2
    exit 1
fi

[ $# -eq 0 ] && set -- system vendor product odm

rc=0
for part in "$@"; do
    case "$part" in
        system|/system|/) mnt=/ ;;
        /*) mnt=$part ;;
        *) mnt=/$part ;;
    esac

    read -r dev fstype opts <<EOF
$(find_mount "$mnt")
EOF
    if [ -z "$dev" ]; then
        echo "remount: $mnt is not mounted" >&2
        rc=1
        continue
    fi
    if [ "$fstype" != ext4 ]; then
        echo "remount: $mnt is $fstype and cannot be made writable" >&2
        rc=1
        continue
    fi

    case ",$opts," in
        *,rw,*) ;;
        *)
            # The logical partitions are mapped read-only; lift that before the remount.
            blockdev --setrw "$dev"
            if ! mount -o remount,rw "$mnt"; then
                echo "remount: failed to remount $mnt" >&2
                rc=1
                continue
            fi
            ;;
    esac

    read -r free bsize <<EOF
$(stat -f -c '%f %S' "$mnt")
EOF
    free_mb=$((free * bsize / 1048576))
    echo "Remounted $mnt as RW ($free_mb MB free)"
    if [ "$free_mb" -lt 8 ]; then
        echo "remount: $mnt has no reserve space; pushes to it will fail" >&2
    fi
done

if [ $rc -eq 0 ]; then
    echo "Remount succeeded"
else
    echo "Remount failed" >&2
fi

if $reboot_after; then
    sync
    reboot
fi
exit $rc
