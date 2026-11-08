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

import android.content.Context;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.RouteInfo;
import android.net.wifi.ScanResult;
import android.net.wifi.WifiConfiguration;
import android.net.wifi.WifiInfo;
import android.net.wifi.WifiManager;
import android.net.wifi.WifiSsid;
import android.os.Build;
import android.system.ErrnoException;
import android.system.Os;
import android.util.Log;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;

import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * Hands the saved Wi-Fi networks over to Abyss: /data/misc/abyss/handoff/wifi.json, written right
 * before entering (while the Wi-Fi service still runs), read by abyssd (abyssd/src/wifi/
 * handoff.rs has the format), deleted by {@link BootReceiver} after the return because it holds
 * passphrases. Mode 0640 system:system (setgid dir), so abyssd (uid root, group system, no
 * CAP_DAC_OVERRIDE) can read it and no app can.
 *
 * <p>What goes in, per saved network: SSID (raw bytes as hex), hidden flag, security (open, owe,
 * psk, sae; enterprise and WEP are left out), passphrase, the WPA PMK (PBKDF2, so abyssd's
 * supplicant skips 4096 rounds per connect), Android's per-network MAC (or "factory"), and the
 * strongest BSSID + frequency from the last scan. Plus the network Android is on right now with
 * its address (for DHCP INIT-REBOOT), the country code and Android's DHCP vendor class.
 *
 * <p>Never throws; a failed hand-over only means Abyss has no Wi-Fi.
 */
final class WifiHandoff {
    private static final String TAG = "AbyssEntry";
    static final String FILE = "/data/misc/abyss/handoff/wifi.json";
    /** PBKDF2 costs ~8k HMAC-SHA1 per network; the rest get the passphrase only. */
    private static final int MAX_PMK = 8;
    private static final int MAX_NETWORKS = 32;

    private WifiHandoff() {}

    static void write(Context c) {
        try {
            JSONObject root = build(c);
            if (root == null) {
                return;
            }
            File f = new File(FILE);
            File tmp = new File(FILE + ".tmp");
            try (FileOutputStream out = new FileOutputStream(tmp)) {
                out.write(root.toString().getBytes(StandardCharsets.UTF_8));
                out.getFD().sync();
            }
            Os.chmod(tmp.getPath(), 0640);
            if (!tmp.renameTo(f)) {
                throw new IOException("rename " + tmp + " -> " + f);
            }
            Log.i(TAG, "wifi hand-over: " + root.getJSONArray("networks").length() + " networks");
        } catch (IOException | JSONException | ErrnoException | RuntimeException e) {
            Log.w(TAG, "wifi hand-over failed; Abyss will have no Wi-Fi", e);
        }
    }

    /** After the return: the passphrases leave /data/misc/abyss again. */
    static void clear() {
        File f = new File(FILE);
        if (f.exists() && !f.delete()) {
            Log.w(TAG, "could not delete " + FILE);
        }
    }

    private static JSONObject build(Context c) throws JSONException {
        WifiManager wm = c.getSystemService(WifiManager.class);
        if (wm == null) {
            return null;
        }
        List<WifiConfiguration> configs = wm.getPrivilegedConfiguredNetworks();
        if (configs == null) {
            Log.w(TAG, "wifi hand-over: no access to the saved networks");
            return null;
        }
        // Strongest scan result per SSID (hex).
        Map<String, ScanResult> best = new HashMap<>();
        List<ScanResult> scans = wm.getScanResults();
        if (scans != null) {
            for (ScanResult r : scans) {
                WifiSsid ws = r.getWifiSsid();
                if (ws == null) {
                    continue;
                }
                String hex = hex(ws.getBytes());
                ScanResult old = best.get(hex);
                if (old == null || r.level > old.level) {
                    best.put(hex, r);
                }
            }
        }
        JSONObject root = new JSONObject();
        root.put("v", 1);
        root.put("written_ms", System.currentTimeMillis());
        root.put("dhcp_vendor_class", "android-dhcp-" + Build.VERSION.RELEASE);
        try {
            String cc = wm.getCountryCode();
            if (cc != null) {
                root.put("country", cc.toUpperCase(Locale.ROOT));
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "wifi hand-over: no country code", e);
        }

        WifiInfo info = wm.getConnectionInfo();
        int currentId = info != null ? info.getNetworkId() : -1;
        JSONObject current = null;
        List<WifiConfiguration> ordered = new ArrayList<>();
        for (WifiConfiguration w : configs) {
            if (w.networkId == currentId) {
                ordered.add(0, w);
            } else {
                ordered.add(w);
            }
        }

        JSONArray nets = new JSONArray();
        int pmks = 0;
        for (WifiConfiguration w : ordered) {
            if (nets.length() >= MAX_NETWORKS) {
                break;
            }
            byte[] ssid = ssidBytes(w.SSID);
            if (ssid == null || ssid.length == 0 || ssid.length > 32) {
                continue;
            }
            JSONArray sec = security(w);
            if (sec.length() == 0) {
                continue;
            }
            String hex = hex(ssid);
            JSONObject n = new JSONObject();
            n.put("ssid", new String(ssid, StandardCharsets.UTF_8));
            n.put("ssid_hex", hex);
            n.put("hidden", w.hiddenSSID);
            n.put("security", sec);
            String psk = w.preSharedKey;
            if (psk != null && psk.length() > 2 && psk.startsWith("\"") && psk.endsWith("\"")) {
                String pass = psk.substring(1, psk.length() - 1);
                n.put("passphrase", pass);
                if (has(sec, "psk") && pmks < MAX_PMK) {
                    try {
                        n.put("pmk", hex(pbkdf2Sha1(pass.getBytes(StandardCharsets.UTF_8), ssid, 4096, 32)));
                        pmks++;
                    } catch (GeneralSecurityException | RuntimeException e) {
                        Log.w(TAG, "wifi hand-over: PMK", e);
                    }
                }
            } else if (psk != null && psk.length() == 64) {
                // Stored as a raw PMK already (no passphrase known: WPA-PSK only).
                n.put("pmk", psk.toLowerCase(Locale.ROOT));
            }
            if (w.macRandomizationSetting == WifiConfiguration.RANDOMIZATION_NONE) {
                n.put("mac_policy", "factory");
            } else {
                n.put("mac_policy", "randomized");
                n.put("mac", w.getRandomizedMacAddress().toString());
            }
            ScanResult r = best.get(hex);
            if (r != null) {
                n.put("bssid", r.BSSID);
                n.put("freq", r.frequency);
            }
            nets.put(n);
            if (w.networkId == currentId && info != null) {
                current = new JSONObject();
                current.put("ssid_hex", hex);
                if (info.getBSSID() != null) {
                    current.put("bssid", info.getBSSID());
                }
                current.put("freq", info.getFrequency());
                if (info.getMacAddress() != null) {
                    current.put("mac", info.getMacAddress());
                }
                addLink(c, current);
            }
        }
        root.put("networks", nets);
        root.put("current", current != null ? current : JSONObject.NULL);
        return root;
    }

    /** The Wi-Fi network's IPv4 address, prefix, gateway and DNS, from ConnectivityManager. */
    private static void addLink(Context c, JSONObject cur) throws JSONException {
        ConnectivityManager cm = c.getSystemService(ConnectivityManager.class);
        if (cm == null) {
            return;
        }
        for (Network net : cm.getAllNetworks()) {
            NetworkCapabilities nc = cm.getNetworkCapabilities(net);
            if (nc == null || !nc.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
                continue;
            }
            LinkProperties lp = cm.getLinkProperties(net);
            if (lp == null) {
                continue;
            }
            for (LinkAddress la : lp.getLinkAddresses()) {
                if (la.getAddress() instanceof Inet4Address) {
                    cur.put("ip", la.getAddress().getHostAddress());
                    cur.put("prefix", la.getPrefixLength());
                    break;
                }
            }
            for (RouteInfo ri : lp.getRoutes()) {
                if (ri.isDefaultRoute() && ri.getGateway() instanceof Inet4Address) {
                    cur.put("gateway", ri.getGateway().getHostAddress());
                    break;
                }
            }
            JSONArray dns = new JSONArray();
            for (InetAddress a : lp.getDnsServers()) {
                if (a instanceof Inet4Address) {
                    dns.put(a.getHostAddress());
                }
            }
            cur.put("dns", dns);
            return;
        }
    }

    private static boolean has(JSONArray a, String v) {
        for (int i = 0; i < a.length(); i++) {
            if (v.equals(a.optString(i))) {
                return true;
            }
        }
        return false;
    }

    /** What abyssd can join: open, owe, psk, sae. Enterprise / WEP / WAPI give an empty list. */
    private static JSONArray security(WifiConfiguration w) {
        JSONArray a = new JSONArray();
        java.util.BitSet km = w.allowedKeyManagement;
        if (km.get(WifiConfiguration.KeyMgmt.WPA_EAP) || km.get(WifiConfiguration.KeyMgmt.IEEE8021X)) {
            return a;
        }
        if (w.wepKeys != null && w.wepKeys[0] != null) {
            return a;
        }
        if (km.get(WifiConfiguration.KeyMgmt.WPA_PSK)) {
            a.put("psk");
        }
        if (km.get(WifiConfiguration.KeyMgmt.SAE)) {
            a.put("sae");
        }
        if (a.length() == 0 && km.get(WifiConfiguration.KeyMgmt.OWE)) {
            a.put("owe");
        }
        if (a.length() == 0 && (km.isEmpty() || km.get(WifiConfiguration.KeyMgmt.NONE))) {
            a.put("open");
        }
        return a;
    }

    /** WifiConfiguration.SSID is "\"name\"" (UTF-8) or bare hex for other byte strings. */
    private static byte[] ssidBytes(String s) {
        if (s == null) {
            return null;
        }
        if (s.length() >= 2 && s.startsWith("\"") && s.endsWith("\"")) {
            return s.substring(1, s.length() - 1).getBytes(StandardCharsets.UTF_8);
        }
        if (s.length() % 2 != 0 || !s.matches("[0-9a-fA-F]+")) {
            return null;
        }
        byte[] b = new byte[s.length() / 2];
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
        }
        return b;
    }

    private static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) {
            sb.append(String.format(Locale.ROOT, "%02x", x & 0xff));
        }
        return sb.toString();
    }

    /** PBKDF2-HMAC-SHA1 over raw bytes (IEEE 802.11i PSK = PBKDF2(passphrase, SSID, 4096, 32)). */
    static byte[] pbkdf2Sha1(byte[] pass, byte[] salt, int iterations, int len)
            throws GeneralSecurityException {
        Mac mac = Mac.getInstance("HmacSHA1");
        mac.init(new SecretKeySpec(pass, "HmacSHA1"));
        byte[] out = new byte[len];
        int done = 0;
        for (int block = 1; done < len; block++) {
            mac.update(salt);
            mac.update(new byte[] {
                (byte) (block >>> 24), (byte) (block >>> 16), (byte) (block >>> 8), (byte) block});
            byte[] u = mac.doFinal();
            byte[] t = u.clone();
            for (int i = 1; i < iterations; i++) {
                u = mac.doFinal(u);
                for (int j = 0; j < t.length; j++) {
                    t[j] ^= u[j];
                }
            }
            int n = Math.min(t.length, len - done);
            System.arraycopy(t, 0, out, done, n);
            done += n;
        }
        return out;
    }
}
