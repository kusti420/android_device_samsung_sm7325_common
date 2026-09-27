#
# Copyright (C) 2023-2025 The LineageOS Project
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#      http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

DEVICE_PACKAGE_OVERLAYS += $(LOCAL_PATH)/overlay

# Add common definitions for Qualcomm
$(call soong_config_set,rfs,mpss_firmware_symlink_target,firmware_modem)
$(call inherit-product, hardware/qcom-caf/common/common.mk)

# Partitions
PRODUCT_PACKAGES += \
    vendor_dsp_mountpoint \
    vendor_firmware_mnt_mountpoint \
    vendor_firmware-modem_mountpoint

PRODUCT_BUILD_SUPER_PARTITION := false
PRODUCT_USE_DYNAMIC_PARTITIONS := true

PRODUCT_ENFORCE_RRO_TARGETS := *

# VNDK
PRODUCT_TARGET_VNDK_VERSION := 30

# No A/B
AB_OTA_UPDATER := false

# Kernel
PRODUCT_ENABLE_UFFD_GC := false

# Init files and fstab
PRODUCT_PACKAGES += \
    fstab.ramplus \
    init.audio.samsung.rc \
    init.fingerprint.rc \
    init.nfc.samsung.rc \
    init.qcom.rc \
    init.qcom.recovery.rc \
    init.qti.kernel.rc \
    init.qti.media.rc \
    init.ramplus.rc \
    init.samsung.bsp.rc \
    init.samsung.display.rc \
    init.samsung.rc \
    init.target.rc \
    init.vendor.onebinary.rc \
    vendor.samsung.rilchip.qcom.rc \
    init.vendor.rilcommon.rc \
    init.vendor.sensors.rc \
    ueventd.qcom.rc \
    wifi_qcom_wcn6750.rc \
    wifi_sec.rc

$(call soong_config_set,libinit,vendor_init_lib,//$(LOCAL_PATH):libinit_samsung_sm7325)

# Vendor scripts
PRODUCT_PACKAGES += \
    init.class_main.sh \
    init.kernel.post_boot.sh \
    init.kernel.post_boot-yupik.sh \
    init.qcom.class_core.sh \
    init.qcom.early_boot.sh \
    init.ichthys.crashlog.rc \
    init.ichthys.crashlog.sh \
    init.qcom.post_boot.sh \
    init.qcom.sh \
    init.qti.kernel.sh \
    init.qti.media.sh \
    vendor_modprobe.sh \
    init.qti.chg_policy.sh \
    init.qti.qcv.sh

PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/init/fstab.qcom:$(TARGET_COPY_OUT_VENDOR_RAMDISK)/first_stage_ramdisk/fstab.qcom \
    $(LOCAL_PATH)/init/fstab.qcom:$(TARGET_COPY_OUT_VENDOR)/etc/fstab.qcom

# IchthysOS custom (fish) boot animation. Copied directly to product/media so it does
# not depend on vendor/lineage/bootanimation (which we neutralized -- it needs
# ImageMagick mogrify). BootAnimation searches /product/media first.
PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/bootanimation.zip:$(TARGET_COPY_OUT_PRODUCT)/media/bootanimation.zip

# Audio
PRODUCT_PACKAGES += \
    android.hardware.audio.service \
    android.hardware.audio@7.0-impl.samsung-sm7325 \
    android.hardware.audio.effect@7.0-impl \
    audio.r_submix.default \
    audio.usb.default \
    libqcomvisualizer \
    libqcomvoiceprocessing \
    libqcompostprocbundle \
    libvolumelistener \
    libqti_vndfwk_detect.vendor:32

# IchthysOS: per-ear hearing-compensation effect, loaded from /vendor/lib*/soundfx via
# audio_effects.xml above.
PRODUCT_PACKAGES += \
    libichthyshearing

# QCOM audio source modules (effects bundle, visualizer, voiceproc) read these
# from the qtiaudio soong namespace. The board sets the make-vars
# AUDIO_FEATURE_ENABLED_GKI/INSTANCE_ID (BoardConfigCommon), but the audio tree
# has no make->soong translation, so set them here to match the vendor blob HAL.
# GKI: our kernel's techpack audio_effects.h uses AUDIO_EFFECTS_CONFIG_SET (GKI naming).
$(call soong_config_set,qtiaudio,feature_gki,true)
$(call soong_config_set,qtiaudio,feature_instance_id,true)
$(call soong_config_set,qtiaudio,feature_extended_compress_format,true)

PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/audio/configs/audio_effects.xml:$(TARGET_COPY_OUT_VENDOR)/etc/audio_effects.xml \
    $(LOCAL_PATH)/audio/configs/audio_io_policy.conf:$(TARGET_COPY_OUT_VENDOR)/etc/audio_io_policy.conf\
    $(LOCAL_PATH)/audio/configs/audio_platform_info.xml:$(TARGET_COPY_OUT_VENDOR)/etc/audio_platform_info.xml \
    $(LOCAL_PATH)/audio/configs/audio_platform_info_intcodec.xml:$(TARGET_COPY_OUT_VENDOR)/etc/audio_platform_info_intcodec.xml \
    $(LOCAL_PATH)/audio/configs/audio_policy_configuration.xml:$(TARGET_COPY_OUT_VENDOR)/etc/audio_policy_configuration.xml \
    $(LOCAL_PATH)/audio/configs/mixer_usb_default.xml:$(TARGET_COPY_OUT_VENDOR)/etc/mixer_usb_default.xml \
    frameworks/av/services/audiopolicy/config/audio_policy_volumes.xml:$(TARGET_COPY_OUT_VENDOR)/etc/audio_policy_volumes.xml \
    frameworks/av/services/audiopolicy/config/bluetooth_audio_policy_configuration_7_0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/bluetooth_audio_policy_configuration_7_0.xml \
    frameworks/av/services/audiopolicy/config/default_volume_tables.xml:$(TARGET_COPY_OUT_VENDOR)/etc/default_volume_tables.xml \
    frameworks/av/services/audiopolicy/config/r_submix_audio_policy_configuration.xml:$(TARGET_COPY_OUT_VENDOR)/etc/r_submix_audio_policy_configuration.xml

# Bluetooth
PRODUCT_PACKAGES += \
    audio.bluetooth.default \
    android.hardware.bluetooth.audio-impl

# Camera
PRODUCT_PACKAGES += \
    android.hardware.camera.provider-service.samsung

$(call soong_config_set_bool,samsungCameraVars,needs_sec_reserved_field,true)
$(call soong_config_set,samsungCameraVars,extra_ids,54) # ID=54 is macro

# CAS
PRODUCT_PACKAGES += \
    android.hardware.cas@1.2-service

# Charger
PRODUCT_PACKAGES += \
    charger_res_images_vendor

# Graphics
PRODUCT_AAPT_CONFIG := normal
PRODUCT_AAPT_PREF_CONFIG := xxhdpi
# A list of dpis to select prebuilt apk, in precedence order.
PRODUCT_AAPT_PREBUILT_DPI := xxhdpi xhdpi hdpi

# Display
PRODUCT_PACKAGES += \
    vendor.qti.hardware.display.composer-service \
    vendor.qti.hardware.display.allocator-service \
    android.hardware.graphics.mapper@3.0-impl-qti-display \
    android.hardware.graphics.mapper@4.0-impl-qti-display \
    libsdmcore \
    libsdmutils \
    libdrmutils \
    libsdedrm \
    libgpu_tonemapper \
    libmemutils \
    init.qti.display_boot.rc \
    init.qti.display_boot.sh \
    gralloc.default \
    vendor.qti.hardware.memtrack-service \
    AdvancedDisplay

# Dolby
$(call inherit-product-if-exists, vendor/sony/dolby/dolby.mk)

# Doze
PRODUCT_PACKAGES += \
    SamsungDoze

# DRM
PRODUCT_PACKAGES += \
    android.hardware.drm-service.clearkey

# fastbootd
PRODUCT_PACKAGES += \
    fastbootd

# ION
$(call soong_config_set_bool,libion,legacy_impl,true)

# Lineage Health
PRODUCT_PACKAGES += \
    vendor.lineage.health-service.default

$(call soong_config_set,lineage_health,charging_control_charging_path,/sys/class/power_supply/battery/batt_slate_mode)
$(call soong_config_set,lineage_health,charging_control_charging_enabled,0)
$(call soong_config_set,lineage_health,charging_control_charging_disabled,1)
$(call soong_config_set_bool,lineage_health,charging_control_supports_bypass,false)
$(call soong_config_set,lineage_health,fast_charge_node,/sys/class/sec/switch/afc_disable)
$(call soong_config_set,lineage_health,fast_charge_value_none,1)
$(call soong_config_set,lineage_health,fast_charge_value_fast_charge,0)

# Fingerprint
PRODUCT_PACKAGES += \
    android.hardware.biometrics.fingerprint-service.samsung

# FlipFlap
PRODUCT_PACKAGES += \
    FlipFlap

# Gatekeeper — Samsung hardware (TEE), so auth tokens are HMAC'd with the TEE key
# that Samsung's fingerprint TA can validate
PRODUCT_PACKAGES += \
    android.hardware.gatekeeper@1.0-impl \
    android.hardware.gatekeeper@1.0-service

# Health
PRODUCT_PACKAGES += \
    android.hardware.health-service.samsung \
    android.hardware.health-service.samsung-recovery

# HotwordEnrollement app permissions
#
# IchthysOS: NOT installed. This file pre-grants KEYPHRASE_ENROLLMENT_APPLICATION and
# MANAGE_VOICE_KEYPHRASES to Google's hotword-enrollment privileged apps
# (com.android.hotwordenrollment.okgoogle / .xgoogle), i.e. it is the allowlist that lets
# "Hey Google" always-on keyphrase detection be enrolled against the SoundTrigger HAL.
# The user does not want voice/hotword features. Since tracker#8 the apps themselves, their
# util jar, the SoundTrigger HAL (soundtrigger@2.3-impl + sound_trigger.primary.lahaina) and
# its configs are gone too, so there is nothing left for this allowlist to apply to.
#
# Restoring it is a one-line uncomment; the file itself is kept at
# configs/permissions/privapp-permissions-hotword.xml.
# PRODUCT_COPY_FILES += \
#     $(LOCAL_PATH)/configs/permissions/privapp-permissions-hotword.xml:$(TARGET_COPY_OUT_PRODUCT)/etc/permissions/privapp-permissions-hotword.xml

# IchthysOS: no assistant / voice search. Declares android.software.voice_recognizers
# unavailable, which stops VoiceInteractionManagerService from ever binding a
# VoiceInteractionService (and hence any hotword detector). See the file's own comment.
PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/permissions/ichthysos-no-voice-recognizers.xml:$(TARGET_COPY_OUT_PRODUCT)/etc/permissions/ichthysos-no-voice-recognizers.xml

# Keylayout
PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/keylayout/sec_touchscreen.kl:$(TARGET_COPY_OUT_VENDOR)/usr/keylayout/sec_touchscreen.kl

# Keymaster — Samsung TEE keymaster@4.0 loads skeymast TA for fingerprint HMAC.
# Software KeyMint handles boot; Samsung keymaster provides TEE HMAC for fingerprint.
PRODUCT_PACKAGES += \
    android.hardware.keymaster@4.0-service.samsung
# IchthysOS: removed android.hardware.security.sharedsecret-service.samsung
# The binary was never extracted from stock firmware and the VINTF manifest
# without a binary makes keystore2 retry the binder connection every 6 seconds.
$(call soong_config_set,samsungVars,target_keymaster4_library,//vendor/samsung/sm7325-common:libskeymaster4device)

PRODUCT_PACKAGES += \
    android.hardware.security.keymint-service.nonsecure

# LiveDisplay
PRODUCT_PACKAGES += \
    vendor.lineage.livedisplay-service.samsung-qcom

PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/media/media_codecs_performance_yupik_v0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_performance_yupik_v0.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_performance_yupik_v1.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_performance_yupik_v1.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_yupik_v0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_yupik_v0.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_lahaina.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_lahaina.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_lahaina_vendor.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_lahaina_vendor.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_performance_lahaina.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_performance_lahaina.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_performance_lahaina_vendor.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_performance_lahaina_vendor.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_performance_yupik_iot.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_performance_yupik_iot.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_yupik_iot.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_yupik_iot.xml \
    $(LOCAL_PATH)/configs/media/media_codecs_yupik_v1.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_codecs_yupik_v1.xml \
    $(LOCAL_PATH)/configs/media/media_profiles.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles.xml \
    $(LOCAL_PATH)/configs/media/media_profiles_vendor.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles_vendor.xml \
    $(LOCAL_PATH)/configs/media/media_profiles_V1_0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles_V1_0.xml \
    $(LOCAL_PATH)/configs/media/media_profiles_yupik_v0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles_yupik_v0.xml \
    $(LOCAL_PATH)/configs/media/media_profiles_yupik_v1.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles_yupik_v1.xml \
    $(LOCAL_PATH)/configs/media/media_profiles_lahaina.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles_lahaina.xml \
    $(LOCAL_PATH)/configs/media/media_profiles_lahaina_vendor.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles_lahaina_vendor.xml \
    $(LOCAL_PATH)/configs/media/media_profiles_yupik_iot.xml:$(TARGET_COPY_OUT_VENDOR)/etc/media_profiles_yupik_iot.xml

# NFC
PRODUCT_PACKAGES += \
    libnfc-nci \
    libnfc_nci_jni \
    Tag

ifeq ($(TARGET_HAVE_SEC_NFC),true)
PRODUCT_PACKAGES += \
    android.hardware.nfc-service.sec

PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/nfc/libnfc-sec-vendor.conf:$(TARGET_COPY_OUT_VENDOR)/etc/libnfc-sec-vendor.conf
endif

PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/nfc/libnfc-nci.conf:$(TARGET_COPY_OUT_VENDOR)/etc/libnfc-nci.conf

# Perf
PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/task_profiles.json:$(TARGET_COPY_OUT_VENDOR)/etc/task_profiles.json

# Permissions
PRODUCT_COPY_FILES += \
    frameworks/native/data/etc/android.hardware.audio.low_latency.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.audio.low_latency.xml \
    frameworks/native/data/etc/android.hardware.audio.pro.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.audio.pro.xml \
    frameworks/native/data/etc/android.hardware.bluetooth.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.bluetooth.xml \
    frameworks/native/data/etc/android.hardware.bluetooth_le.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.bluetooth_le.xml \
    frameworks/native/data/etc/android.hardware.camera.flash-autofocus.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.camera.flash-autofocus.xml \
    frameworks/native/data/etc/android.hardware.camera.front.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.camera.front.xml \
    frameworks/native/data/etc/android.hardware.camera.full.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.camera.full.xml \
    frameworks/native/data/etc/android.hardware.camera.raw.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.camera.raw.xml \
    frameworks/native/data/etc/android.hardware.fingerprint.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.fingerprint.xml \
    frameworks/native/data/etc/android.hardware.location.gps.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.location.gps.xml \
    frameworks/native/data/etc/android.hardware.nfc.hcef.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.nfc.hcef.xml \
    frameworks/native/data/etc/android.hardware.nfc.hce.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.nfc.hce.xml \
    frameworks/native/data/etc/android.hardware.nfc.uicc.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.nfc.uicc.xml \
    frameworks/native/data/etc/android.hardware.nfc.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.nfc.xml \
    frameworks/native/data/etc/android.hardware.opengles.aep.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.opengles.aep.xml \
    frameworks/native/data/etc/android.hardware.sensor.accelerometer.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.sensor.accelerometer.xml \
    frameworks/native/data/etc/android.hardware.sensor.compass.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.sensor.compass.xml \
    frameworks/native/data/etc/android.hardware.sensor.gyroscope.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.sensor.gyroscope.xml \
    frameworks/native/data/etc/android.hardware.sensor.light.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.sensor.light.xml \
    frameworks/native/data/etc/android.hardware.sensor.proximity.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.sensor.proximity.xml \
    frameworks/native/data/etc/android.hardware.sensor.stepcounter.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.sensor.stepcounter.xml \
    frameworks/native/data/etc/android.hardware.sensor.stepdetector.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.sensor.stepdetector.xml \
    frameworks/native/data/etc/android.hardware.telephony.gsm.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.telephony.gsm.xml \
    frameworks/native/data/etc/android.hardware.touchscreen.multitouch.jazzhand.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.touchscreen.multitouch.jazzhand.xml \
    frameworks/native/data/etc/android.hardware.usb.accessory.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.usb.accessory.xml \
    frameworks/native/data/etc/android.hardware.usb.host.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.usb.host.xml \
    frameworks/native/data/etc/android.hardware.vulkan.compute-0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.vulkan.compute-0.xml \
    frameworks/native/data/etc/android.hardware.vulkan.level-1.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.vulkan.level-1.xml \
    frameworks/native/data/etc/android.software.vulkan.deqp.level-2020-03-01.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.vulkan.deqp.level.xml \
    frameworks/native/data/etc/android.hardware.vulkan.version-1_1.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.vulkan.version-1_1.xml \
    frameworks/native/data/etc/android.hardware.wifi.aware.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.wifi.aware.xml \
    frameworks/native/data/etc/android.hardware.wifi.direct.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.wifi.direct.xml \
    frameworks/native/data/etc/android.hardware.wifi.passpoint.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.wifi.passpoint.xml \
    frameworks/native/data/etc/android.hardware.wifi.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.wifi.xml \
    frameworks/native/data/etc/android.software.ipsec_tunnels.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.ipsec_tunnels.xml \
    frameworks/native/data/etc/android.software.midi.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.midi.xml \
    frameworks/native/data/etc/android.software.sip.voip.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.sip.voip.xml \
    frameworks/native/data/etc/android.software.verified_boot.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.verified_boot.xml \
    frameworks/native/data/etc/com.nxp.mifare.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/com.nxp.mifare.xml \
    frameworks/native/data/etc/handheld_core_hardware.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/handheld_core_hardware.xml \
    frameworks/native/data/etc/android.software.freeform_window_management.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.freeform_window_management.xml

# Power
PRODUCT_PACKAGES += \
    android.hardware.power-service.pixel-libperfmgr \
    libqti-perfd-client \
    libperfmgr

PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/powerhint.json:$(TARGET_COPY_OUT_VENDOR)/etc/powerhint.json

# RIL
PRODUCT_PACKAGES += \
    secril_config_svc \
    sehradiomanager

# SIM Toolkit (STK) — handles SIM proactive commands (DISPLAY TEXT / GET INPUT).
# Required for SIM-applet auth like Mobile-ID (EE). AOSP ships it via generic_system.mk
# but this product config doesn't inherit that, so add it explicitly.
PRODUCT_PACKAGES += \
    Stk

# Sensors
# IchthysOS/AOSP-17 a52sxq bringup: DISABLED. The multihal is installed and declares
# android.hardware.sensors.ISensors/default via its own vintf_fragment, but it never
# registers (its SSC sub-HALs need the ADSP sensor PD, which fails on this bringup —
# same reason vendor.sensors/sscrpcd is disabled in init.target.rc).
#
# A HAL DECLARED in VINTF but never registered makes its client block FOREVER:
# system_server retried ISensors as a lazy AIDL service once a second for ~95s, never
# finished booting, and was killed -> zygote died 4x -> UPDATABLE_CRASHING -> no boot.
# Undeclared is the fast-fail path (see ILights/IPowerStats/IVibratorManager, which
# system_server skips cleanly), so dropping the package drops the declaration too and
# SensorService comes up with zero sensors. Same lesson as the keymaster@4.0 VINTF decl.
#
# IchthysOS 2026-07-26: RE-ENABLED. The ADSP sensor PD (sscrpcd sensorspd) was verified
# healthy at runtime (sscrpcd stable, all sensor configs load, no SSR/crash) — the original
# bringup crash was transient (empty persist registry on first boot, since populated).
PRODUCT_PACKAGES += \
    android.hardware.sensors-service.samsung-multihal

# Touch features
PRODUCT_PACKAGES += \
    vendor.lineage.touch-service.samsung

# Vendor service manager
PRODUCT_PACKAGES += \
    vndservicemanager

# Update
PRODUCT_SOONG_NAMESPACES += bootable/deprecated-ota

# USB
# IchthysOS/AOSP-17 a52sxq bringup: android.hardware.usb-service.qti was never synced
# (missing from the tree -> silently dropped by ALLOW_MISSING_DEPENDENCIES -> NO USB HAL
# in the image -> UsbDeviceManager in the booted ROM could not configure the configfs
# gadget -> USB/adb never enumerated (recovery's static gadget still worked, which is why
# adb worked there but not in the OS). Use the Samsung AIDL USB HALs from hardware/samsung
# instead: IUsb (port/role) + IUsbGadget (function switching, incl. adb). Both build and
# ship their own VINTF fragments + init.rc; labeled via device file_contexts below.
PRODUCT_PACKAGES += \
    android.hardware.usb-service.samsung \
    android.hardware.usb.gadget-service.samsung \
    init.qcom.usb.rc

# The Samsung gadget HAL bakes in the UDC (USB Device Controller) name at compile time
# via -DGADGET_NAME. On a52sxq (SM7325/lahaina) the DWC3 controller is a600000.dwc3
# (confirmed from /sys/class/udc on device). Without this the HAL fails to compile
# (UsbGadget.h: "use of undeclared identifier 'GADGET_NAME'").
$(call soong_config_set,samsungUsbGadgetVars,gadget_name,a600000.dwc3)
# NOTE: init.qcom.usb.rc / init.qcom.usb.sh and the vendor/qcom/opensource/usb/etc
# soong namespace all came from the never-synced QTI usb repo (silently dropped). The
# Samsung USB HALs ship their own init.rc, and generic Android configfs init (driven by
# sys.usb.configfs=1, already set in init.target.rc) composes the adb/mtp gadget, so the
# QTI-specific bits are not needed for bringup.

# Samsung Ext Flash
PRODUCT_PACKAGES += \
    vendor.samsung_ext.hardware.camera.flashlight-service \
    FlashControl

# Vibrator
PRODUCT_PACKAGES += \
    android.hardware.vibrator-service.samsung

$(call soong_config_set_bool,samsungVibratorVars,duration_amplitude,true)

# Tether
PRODUCT_PACKAGES += \
    ipacm \
    IPACM_cfg.xml

# IchthysOS/AOSP-17: WLAN driver plugin. wpa_supplicant/hostapd need the Qualcomm qcwcn
# nl80211 driver-command lib (lib_driver_cmd_qcwcn) to actually drive the WCN6750 chip;
# without it the daemons build but WiFi never associates (this was stubbed out during
# bringup to get a bootable image). The lib lives in the hardware/qcom-caf/wlan/qcwcn
# soong namespace, so it is referenced by fully-qualified //path:module — the
# default-namespace wpa_supplicant/hostapd cannot resolve a bare cross-namespace name
# (same mechanism as libinit/udfps soong_config_set elsewhere in this tree). The old
# BOARD_WPA_SUPPLICANT_PRIVATE_LIB / BOARD_HOSTAPD_PRIVATE_LIB make-vars are dead in the
# AOSP-17 soong build; these soong configs are what actually wire the lib in.
$(call soong_config_set,wpa_supplicant_8,board_wlan_device,qcwcn)
$(call soong_config_set,wpa_supplicant_8,board_wpa_supplicant_private_lib,//hardware/qcom-caf/wlan/qcwcn:lib_driver_cmd_qcwcn)
$(call soong_config_set_bool,wpa_supplicant_8,board_wpa_supplicant_private_lib_event,true)
# NOTE: AOSP-17 builds wpa_supplicant/hostapd with -DANDROID_LIB_STUB unconditionally, so
# driver_nl80211_android.c provides the P2P helper stubs. The qcwcn private lib is patched
# (driver_cmd_nl80211.c, ICHTHYS_BASE_PROVIDES_P2P_STUBS) to NOT redefine them, so the two
# no longer collide. Do not try to force use_stub_lib off — the lib now depends on the
# base stubs being present.
# NOTE: hostapd deliberately does NOT get lib_driver_cmd_qcwcn. AOSP-17's
# driver_nl80211_android.c compiles stub p2p helpers (wpa_driver_set_p2p_noa etc.) for
# hostapd, which collide at link with the same real symbols in the qcwcn lib. hostapd
# drives SoftAP fine over plain nl80211 without the QCOM driver-command plugin, so it
# stays on the stubs (its known-good pre-WLAN-fix state). Only wpa_supplicant (station
# mode) needs the private lib.

# WiFi
PRODUCT_PACKAGES += \
    android.hardware.wifi-service \
    hostapd \
    libwifi-hal-qcom \
    libwpa_client \
    wpa_cli \
    wpa_supplicant \
    wpa_supplicant.conf \
    WifiOverlay

PRODUCT_COPY_FILES += \
    $(LOCAL_PATH)/configs/wifi/icm.conf:$(TARGET_COPY_OUT_VENDOR)/etc/wifi/icm.conf \
    $(LOCAL_PATH)/configs/wifi/indoorchannel.info:$(TARGET_COPY_OUT_VENDOR)/etc/wifi/indoorchannel.info \
    $(LOCAL_PATH)/configs/wifi/p2p_supplicant_overlay.conf:$(TARGET_COPY_OUT_VENDOR)/etc/wifi/p2p_supplicant_overlay.conf \
    $(LOCAL_PATH)/configs/wifi/wpa_supplicant_overlay.conf:$(TARGET_COPY_OUT_VENDOR)/etc/wifi/wpa_supplicant_overlay.conf \
    $(LOCAL_PATH)/configs/wifi/WCNSS_qcom_cfg.ini:$(TARGET_COPY_OUT_VENDOR)/etc/wifi/WCNSS_qcom_cfg.ini

# WiFi firmware symlinks
PRODUCT_PACKAGES += \
    firmware_qca6750_WCNSS_qcom_cfg.ini_symlink \
    firmware_wlan_WCNSS_qcom_cfg.ini_symlink

# Soong namespaces
PRODUCT_SOONG_NAMESPACES += \
    $(LOCAL_PATH) \
    hardware/google/interfaces \
    hardware/google/pixel \
    hardware/google/pixel/power-libperfmgr \
    hardware/samsung-ext/interfaces \
    hardware/samsung \
    vendor/qcom/opensource/commonsys-intf/display \
    vendor/qcom/opensource/display \
    hardware/qcom-caf/sm8350

# Inherit proprietary blobs
$(call inherit-product, vendor/samsung/sm7325-common/sm7325-common-vendor.mk)

# Set system_server compiler filter to speed-profile
PRODUCT_SYSTEM_SERVER_COMPILER_FILTER := speed-profile

# IchthysOS/AOSP-17: the QCOM display source header_libs are gated on the qtidisplay soong
# config (qtidisplay_cc_defaults). The device already has the display props/packages, so we
# set ONLY the soong config here (inheriting the full display-product.mk duplicated vendor
# props). Values from hardware/qcom-caf/sm8350/display/config/display-product.mk.
SOONG_CONFIG_NAMESPACES += qtidisplay
SOONG_CONFIG_qtidisplay := drmpp headless llvmsa gralloc4 udfps default
SOONG_CONFIG_qtidisplay_drmpp := true
SOONG_CONFIG_qtidisplay_headless := false
SOONG_CONFIG_qtidisplay_llvmsa := false
SOONG_CONFIG_qtidisplay_gralloc4 := true
SOONG_CONFIG_qtidisplay_udfps := false
SOONG_CONFIG_qtidisplay_default := true

# IchthysOS/AOSP-17: the A17 framework compatibility matrices have no kernel entry for 5.4
# (the a52sxq's stock kernel), failing check_vintf. Don't enforce the VINTF kernel-version
# requirement for this bringup (the device kernel is 5.4).
PRODUCT_OTA_ENFORCE_VINTF_KERNEL_REQUIREMENTS := false

# IchthysOS/AOSP-17: the device vendor manifest targets FCM version 5 (Android-12 era), whose
# framework compatibility matrix Android 17 no longer ships, so check_vintf can't run. Disable
# strict VINTF manifest enforcement for this bringup (revisit by bumping the vendor target-level).
PRODUCT_ENFORCE_VINTF_MANIFEST := false

# IchthysOS/AOSP-17 a52sxq bringup: bake host adb key so adbd auto-authorizes during
# the (currently headless/GPU-broken) boot — enables live logcat/dmesg without on-screen
# "Allow USB debugging". Remove once GPU/display bring-up is done.
PRODUCT_ADB_KEYS := device/samsung/sm7325-common/adbkey.pub
