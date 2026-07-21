#!/vendor/bin/sh
#
# IchthysOS/AOSP-17 a52sxq bringup: /dev/ion keeper.
#
# The kernel ION misc device (major 10) is registered fine (heaps created), and
# ueventd creates /dev/ion early. But on this AOSP-17 build the /dev/ion node is
# torn down mid-boot (legacy ION is deprecated in newer Android) while our kernel is
# ION-only (no dma-buf heaps) and the Adreno GPU + gralloc blobs require /dev/ion.
# Result: SurfaceFlinger's RenderEngine gets "open(/dev/ion) errno 2" -> EGL_BAD_ALLOC
# -> SIGABRT -> boot hangs. The kernel device stays registered, so simply recreating
# the /dev node makes open()/ioctl succeed. Keep it present for the life of the system.
#
# TODO(bringup): identify and stop whatever tears the node down, then drop this keeper.

ion_sysfs=/sys/class/misc/ion/dev

while true; do
    if [ ! -e /dev/ion ] && [ -f "$ion_sysfs" ]; then
        d=$(cat "$ion_sysfs" 2>/dev/null)
        if [ -n "$d" ]; then
            mknod /dev/ion c "${d%%:*}" "${d##*:}" 2>/dev/null
            chmod 0666 /dev/ion 2>/dev/null
            chown system system /dev/ion 2>/dev/null
        fi
    fi
    sleep 1
done
