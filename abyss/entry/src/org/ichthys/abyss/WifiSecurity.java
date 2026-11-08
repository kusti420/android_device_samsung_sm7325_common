/*
 * Copyright (C) 2026 IchthysOS
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.ichthys.abyss;

import java.util.ArrayList;
import java.util.BitSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * The security part of the Wi-Fi hand-over, without Android classes (host-tested by
 * tests/WifiSecurityTest.java).
 *
 * <p>Why it exists: {@code WifiManager.getPrivilegedConfiguredNetworks()} does not return one
 * WifiConfiguration per saved network. Since Android 12 the service splits every multi-type
 * network into one legacy copy per security type, all with the same network id
 * (WifiConfigurationUtil.convertMultiTypeConfigsToLegacyConfigs): a "WPA2/WPA3" home network
 * comes back as an SAE copy and a PSK copy, in the order of its security-params list (SAE first
 * when Android upgraded an existing PSK network). The hand-over took the first copy per SSID
 * and lost WPA2. {@link Merge} puts the copies back together by network id.
 *
 * <p>The type of one copy comes from SecurityParams (hidden; read by reflection in
 * WifiHandoff) or, failing that, from the legacy bit sets with the same rules as
 * WifiConfiguration.convertLegacyFieldsToSecurityParamsIfNeeded.
 */
final class WifiSecurity {
    private WifiSecurity() {}

    // WifiConfiguration.SECURITY_TYPE_*.
    static final int OPEN = 0;
    static final int WEP = 1;
    static final int PSK = 2;
    static final int EAP = 3;
    static final int SAE = 4;
    static final int EAP_SUITE_B = 5;
    static final int OWE = 6;
    static final int WAPI_PSK = 7;
    static final int WAPI_CERT = 8;
    static final int EAP_WPA3 = 9;
    static final int OSEN = 10;
    static final int PASSPOINT_R1_R2 = 11;
    static final int PASSPOINT_R3 = 12;
    static final int DPP = 13;

    // WifiConfiguration.KeyMgmt bits.
    static final int KM_NONE = 0;
    static final int KM_WPA_PSK = 1;
    static final int KM_WPA_EAP = 2;
    static final int KM_IEEE8021X = 3;
    static final int KM_WPA2_PSK = 4;
    static final int KM_OSEN = 5;
    static final int KM_FT_PSK = 6;
    static final int KM_FT_EAP = 7;
    static final int KM_SAE = 8;
    static final int KM_OWE = 9;
    static final int KM_SUITE_B_192 = 10;
    static final int KM_WPA_PSK_SHA256 = 11;
    static final int KM_WPA_EAP_SHA256 = 12;
    static final int KM_WAPI_PSK = 13;
    static final int KM_WAPI_CERT = 14;
    static final int KM_FILS_SHA256 = 15;
    static final int KM_FILS_SHA384 = 16;
    static final int KM_DPP = 17;

    // WifiConfiguration.Protocol / PairwiseCipher / GroupCipher bits used below.
    static final int PROTO_WPA = 0;
    static final int PROTO_RSN = 1;
    static final int PAIRWISE_TKIP = 1;
    static final int GROUP_TKIP = 2;

    /** The hand-over name of an Android security type, or null for one it does not know. */
    static String name(int type) {
        switch (type) {
            case OPEN: return "open";
            case WEP: return "wep";
            case PSK: return "psk";
            case EAP: return "eap";
            case SAE: return "sae";
            case EAP_SUITE_B: return "eap_suiteb";
            case OWE: return "owe";
            case WAPI_PSK: return "wapi_psk";
            case WAPI_CERT: return "wapi_cert";
            case EAP_WPA3: return "eap_wpa3";
            case OSEN: return "osen";
            // A Passpoint network the framework created: an ordinary 802.1X network to the
            // supplicant (R3 requires PMF, like WPA3-Enterprise).
            case PASSPOINT_R1_R2: return "eap";
            case PASSPOINT_R3: return "eap_wpa3";
            case DPP: return "dpp";
            default: return null;
        }
    }

    /**
     * The security type of one legacy copy from its bit sets (WifiConfiguration
     * .convertLegacyFieldsToSecurityParamsIfNeeded + isWpa3EnterpriseConfiguration).
     */
    static int fromLegacy(BitSet km, BitSet proto, BitSet pairwise, BitSet group,
            boolean requirePmf, boolean hasWepKeys) {
        if (km.get(KM_WAPI_CERT)) return WAPI_CERT;
        if (km.get(KM_WAPI_PSK)) return WAPI_PSK;
        if (km.get(KM_SUITE_B_192)) return EAP_SUITE_B;
        if (km.get(KM_DPP)) return DPP;
        if (km.get(KM_OWE)) return OWE;
        if (km.get(KM_SAE)) return SAE;
        if (km.get(KM_OSEN)) return OSEN;
        if (km.get(KM_WPA2_PSK) || km.get(KM_WPA_PSK_SHA256) || km.get(KM_FT_PSK)) return PSK;
        if (km.get(KM_WPA_EAP) || km.get(KM_FT_EAP) || km.get(KM_IEEE8021X)
                || km.get(KM_WPA_EAP_SHA256) || km.get(KM_FILS_SHA256)
                || km.get(KM_FILS_SHA384)) {
            boolean wpa3 = requirePmf && proto.cardinality() == 1 && proto.get(PROTO_RSN)
                    && !pairwise.get(PAIRWISE_TKIP) && !group.get(GROUP_TKIP);
            return wpa3 ? EAP_WPA3 : EAP;
        }
        if (km.get(KM_WPA_PSK)) return PSK;
        if (km.get(KM_NONE) && hasWepKeys) return WEP;
        return OPEN;
    }

    /** One saved network's legacy copies, merged back by network id. */
    static final class Merge<T> {
        /** The first copy (the one the other fields are read from). */
        final T first;
        /** Enabled types, in the order Android listed the copies. */
        final List<String> on = new ArrayList<>();
        /** Types a transition-disable indication turned off. */
        final List<String> off = new ArrayList<>();
        /** Copies by type name (to read type-specific fields: SAE H2E, FILS, Suite-B). */
        final Map<String, T> byType = new LinkedHashMap<>();

        Merge(T first) {
            this.first = first;
        }

        void add(T copy, int type, boolean enabled) {
            String n = name(type);
            if (n == null) {
                n = "type" + type;
            }
            if (byType.containsKey(n)) {
                return;
            }
            byType.put(n, copy);
            (enabled ? on : off).add(n);
        }
    }

    /**
     * Groups copies by network id, keeping Android's order of networks (first copy wins).
     *
     * @param ids the network id of each copy, same order as {@code copies}
     */
    static <T> List<Merge<T>> merge(List<T> copies, List<Integer> ids, List<Integer> types,
            List<Boolean> enabled) {
        Map<Integer, Merge<T>> m = new LinkedHashMap<>();
        for (int i = 0; i < copies.size(); i++) {
            Merge<T> g = m.get(ids.get(i));
            if (g == null) {
                g = new Merge<>(copies.get(i));
                m.put(ids.get(i), g);
            }
            g.add(copies.get(i), types.get(i), enabled.get(i));
        }
        return new ArrayList<>(m.values());
    }

    /** WifiEnterpriseConfig.Eap.* → the supplicant's method name, or null (none). */
    static String eapMethod(int m) {
        switch (m) {
            case 0: return "PEAP";
            case 1: return "TLS";
            case 2: return "TTLS";
            case 3: return "PWD";
            case 4: return "SIM";
            case 5: return "AKA";
            case 6: return "AKA'";
            case 7: return "WFA-UNAUTH-TLS";
            case 8: return "WAPI-CERT";
            default: return null;
        }
    }

    /** WifiEnterpriseConfig.Phase2.* → the supplicant's phase-2 name ("" = none). */
    static String phase2(int m) {
        switch (m) {
            case 1: return "PAP";
            case 2: return "MSCHAP";
            case 3: return "MSCHAPV2";
            case 4: return "GTC";
            case 5: return "SIM";
            case 6: return "AKA";
            case 7: return "AKA'";
            default: return "";
        }
    }

    /**
     * The CA certificate field the HAL would set for these aliases
     * (WifiEnterpriseConfig.setCaCertificateAliases): one alias = keystore://CACERT_x, several
     * = keystores:// + hex-encoded "CACERT_x" joined by spaces; a "hash://" pin passes as is.
     */
    static String caCertField(String[] aliases) {
        if (aliases == null || aliases.length == 0) {
            return null;
        }
        if (aliases.length == 1) {
            String a = aliases[0];
            if (a == null || a.isEmpty()) return null;
            return a.contains("://") ? a : "keystore://CACERT_" + a;
        }
        StringBuilder sb = new StringBuilder("keystores://");
        for (int i = 0; i < aliases.length; i++) {
            if (i > 0) sb.append(' ');
            byte[] b = ("CACERT_" + aliases[i]).getBytes(java.nio.charset.StandardCharsets.UTF_8);
            for (byte x : b) {
                sb.append(String.format(java.util.Locale.ROOT, "%02x", x & 0xff));
            }
        }
        return sb.toString();
    }

    /** Android's MAC policy for the hand-over (WifiConfigManager.shouldUseNonPersistentRandomization). */
    static String macPolicy(int setting, boolean openNetwork, boolean staticIp,
            boolean everConnected, boolean neverCaptive, boolean allowNonPersistentOnOpen,
            boolean forceNonPersistent) {
        // WifiConfiguration.RANDOMIZATION_NONE / PERSISTENT / NON_PERSISTENT / AUTO.
        if (setting == 0) return "factory";
        if (forceNonPersistent || setting == 2) return "non_persistent";
        if (setting == 1) return "persistent";
        if (staticIp) return "persistent";
        if (openNetwork && allowNonPersistentOnOpen && everConnected && neverCaptive) {
            return "non_persistent";
        }
        return "persistent";
    }
}
