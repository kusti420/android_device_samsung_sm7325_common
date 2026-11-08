# Copyright (C) 2026 IchthysOS
# SPDX-License-Identifier: Apache-2.0
#
# Derives the product apns-conf.xml from AOSP device/sample/etc/apns-full-conf.xml with the IMS
# APN changes VoLTE needs in Estonia. Values follow the operators' entries in the stock A528B
# (A528BXXU5FWK4) apns-conf.xml and imsservice imsprofile.json:
#   Elisa EE (carrier_id 668) IMS: protocol IPV6/roaming IP -> IPV4V6/IPV4V6 (stock: IPV4V6)
#   Tele2 EE (carrier_id 669) IMS: missing in AOSP and in stock apns-conf; stock imsprofile uses
#                                  the well-known "ims" APN (GSMA IR.92) -> added, IPV4V6
# Telia EE (carrier_id 667) already has "ims" (IPV4V6) and is left as is.
# Fails the build if the AOSP file no longer looks as expected, so a changed upstream entry is
# noticed instead of silently patched twice or not at all.

function flush(   b) {
    b = block
    if (b ~ /carrier_id = "668"/ && b ~ /mcc="248"/ && b ~ /type="ims"/) {
        n = sub(/protocol="IPV6"\n/, "protocol=\"IPV4V6\"\n      roaming_protocol=\"IPV4V6\"\n", b)
        if (n != 1 || b ~ /roaming_protocol="IPV6"/) { print "apns_ims_ee: unexpected Elisa EE IMS entry" > "/dev/stderr"; bad = 1 }
        elisa++
    }
    if (b ~ /carrier_id = "669"/ && b ~ /type="ims"/) {
        print "apns_ims_ee: AOSP now has a Tele2 EE IMS APN; drop the addition" > "/dev/stderr"; bad = 1
    }
    printf "%s", b
    if (b ~ /carrier="Tele2 MMS"/ && b ~ /carrier_id = "669"/) {
        print ""
        print "  <apn carrier=\"Tele2 IMS\""
        print "      carrier_id = \"669\""
        print "      mcc=\"248\""
        print "      mnc=\"03\""
        print "      apn=\"ims\""
        print "      type=\"ims\""
        print "      protocol=\"IPV4V6\""
        print "      roaming_protocol=\"IPV4V6\""
        print "      user_visible=\"false\""
        print "  />"
        tele2++
    }
    block = ""
    inblock = 0
}

{
    if (!inblock && $0 ~ /^[ \t]*<apn[ \t]/) inblock = 1
    if (inblock) {
        block = block $0 "\n"
        if ($0 ~ /\/>[ \t]*$/) flush()
        next
    }
    print
}

END {
    if (inblock) { print "apns_ims_ee: unterminated <apn>" > "/dev/stderr"; exit 1 }
    if (elisa != 1 || tele2 != 1 || bad) {
        print "apns_ims_ee: elisa=" elisa " tele2=" tele2 " bad=" bad > "/dev/stderr"
        exit 1
    }
}
