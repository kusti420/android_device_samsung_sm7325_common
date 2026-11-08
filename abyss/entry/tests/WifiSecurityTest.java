/*
 * Copyright (C) 2026 IchthysOS
 * Licensed under the Apache License, Version 2.0.
 */
package org.ichthys.abyss;

import java.util.Arrays;
import java.util.BitSet;
import java.util.List;

/** Host test of the Wi-Fi hand-over's security merge (no Android). Run with tests/run_wifi.sh. */
public final class WifiSecurityTest {
    static int failures;

    static void check(boolean ok, String what) {
        System.out.println((ok ? "ok   " : "FAIL ") + what);
        if (!ok) {
            failures++;
        }
    }

    static BitSet bits(int... b) {
        BitSet s = new BitSet();
        for (int x : b) {
            s.set(x);
        }
        return s;
    }

    public static void main(String[] args) {
        // The owner's router: Android upgraded a PSK network to SAE, so WifiManager hands out
        // the SAE copy first, then the PSK copy, same network id. Plus an unrelated open network.
        List<String> copies = Arrays.asList("home-sae", "home-psk", "cafe-open", "cafe-owe");
        List<WifiSecurity.Merge<String>> m = WifiSecurity.merge(copies,
                Arrays.asList(7, 7, 9, 9),
                Arrays.asList(WifiSecurity.SAE, WifiSecurity.PSK, WifiSecurity.OPEN,
                        WifiSecurity.OWE),
                Arrays.asList(true, true, true, true));
        check(m.size() == 2, "copies merged by network id: " + m.size());
        check(m.get(0).on.equals(Arrays.asList("sae", "psk")), "home keeps SAE and WPA2: " + m.get(0).on);
        check("home-sae".equals(m.get(0).first), "first copy is the base");
        check("home-psk".equals(m.get(0).byType.get("psk")), "per-type copy kept");
        check(m.get(1).on.equals(Arrays.asList("open", "owe")), "open + OWE upgrade");

        // Transition disable: the PSK copy comes back disabled.
        m = WifiSecurity.merge(Arrays.asList("a", "b"), Arrays.asList(3, 3),
                Arrays.asList(WifiSecurity.PSK, WifiSecurity.SAE), Arrays.asList(false, true));
        check(m.get(0).on.equals(Arrays.asList("sae")) && m.get(0).off.equals(Arrays.asList("psk")),
                "transition-disabled PSK listed as off");

        // Legacy bit fallback (WifiConfiguration.convertLegacyFieldsToSecurityParamsIfNeeded).
        BitSet none = new BitSet();
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_WPA_PSK), none, none, none, false, false)
                == WifiSecurity.PSK, "WPA_PSK → psk");
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_SAE), none, none, none, true, false)
                == WifiSecurity.SAE, "SAE → sae");
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_OWE), none, none, none, true, false)
                == WifiSecurity.OWE, "OWE → owe");
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_NONE), none, none, none, false, true)
                == WifiSecurity.WEP, "NONE + WEP keys → wep");
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_NONE), none, none, none, false, false)
                == WifiSecurity.OPEN, "NONE → open");
        check(WifiSecurity.fromLegacy(none, none, none, none, false, false) == WifiSecurity.OPEN,
                "empty → open");
        BitSet eap = bits(WifiSecurity.KM_WPA_EAP, WifiSecurity.KM_IEEE8021X);
        check(WifiSecurity.fromLegacy(eap, bits(WifiSecurity.PROTO_WPA, WifiSecurity.PROTO_RSN),
                bits(WifiSecurity.PAIRWISE_TKIP), none, false, false) == WifiSecurity.EAP,
                "WPA_EAP → eap");
        check(WifiSecurity.fromLegacy(eap, bits(WifiSecurity.PROTO_RSN), none, none, true, false)
                == WifiSecurity.EAP_WPA3, "WPA_EAP + RSN-only + PMF, no TKIP → eap_wpa3");
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_WPA_EAP, WifiSecurity.KM_IEEE8021X,
                WifiSecurity.KM_SUITE_B_192), bits(WifiSecurity.PROTO_RSN), none, none, true, false)
                == WifiSecurity.EAP_SUITE_B, "SUITE_B_192 → eap_suiteb");
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_WAPI_PSK), none, none, none, false, false)
                == WifiSecurity.WAPI_PSK, "WAPI_PSK → wapi_psk");
        check(WifiSecurity.fromLegacy(bits(WifiSecurity.KM_DPP), none, none, none, true, false)
                == WifiSecurity.DPP, "DPP → dpp");
        check("eap".equals(WifiSecurity.name(WifiSecurity.PASSPOINT_R1_R2))
                && "eap_wpa3".equals(WifiSecurity.name(WifiSecurity.PASSPOINT_R3)),
                "Passpoint networks map to 802.1X");

        check("PEAP".equals(WifiSecurity.eapMethod(0)) && "AKA'".equals(WifiSecurity.eapMethod(6))
                && WifiSecurity.eapMethod(-1) == null, "EAP method names");
        check("MSCHAPV2".equals(WifiSecurity.phase2(3)) && "".equals(WifiSecurity.phase2(0)),
                "phase 2 names");
        check("keystore://CACERT_corp".equals(WifiSecurity.caCertField(new String[] {"corp"})),
                "one CA alias");
        check(("keystores://" + "4341434552545f61" + " " + "4341434552545f62")
                .equals(WifiSecurity.caCertField(new String[] {"a", "b"})), "two CA aliases");
        check("hash://server/sha256/ab".equals(
                WifiSecurity.caCertField(new String[] {"hash://server/sha256/ab"})), "pinned hash");
        check(WifiSecurity.caCertField(null) == null, "no CA");

        check("factory".equals(WifiSecurity.macPolicy(0, false, false, true, true, true, true)),
                "RANDOMIZATION_NONE → factory");
        check("persistent".equals(WifiSecurity.macPolicy(1, true, false, true, true, true, false)),
                "PERSISTENT → persistent");
        check("non_persistent".equals(WifiSecurity.macPolicy(2, false, false, false, false, true, false)),
                "NON_PERSISTENT → non_persistent");
        check("non_persistent".equals(WifiSecurity.macPolicy(3, true, false, true, true, true, false)),
                "AUTO, open, connected, no portal → non_persistent");
        check("persistent".equals(WifiSecurity.macPolicy(3, true, true, true, true, true, false)),
                "AUTO with a static IP → persistent");
        check("persistent".equals(WifiSecurity.macPolicy(3, false, false, true, true, true, false)),
                "AUTO, secured → persistent");

        System.out.println(failures == 0 ? "ALL OK" : failures + " FAILED");
        System.exit(failures == 0 ? 0 : 1);
    }
}
