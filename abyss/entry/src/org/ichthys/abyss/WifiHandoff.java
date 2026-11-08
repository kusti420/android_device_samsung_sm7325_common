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
import android.net.IpConfiguration;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.RouteInfo;
import android.net.StaticIpConfiguration;
import android.net.wifi.ScanResult;
import android.net.wifi.WifiConfiguration;
import android.net.wifi.WifiEnterpriseConfig;
import android.net.wifi.WifiInfo;
import android.net.wifi.WifiManager;
import android.net.wifi.WifiSsid;
import android.os.Build;
import android.provider.Settings;
import android.system.ErrnoException;
import android.system.Os;
import android.util.Log;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.lang.reflect.Method;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * Hands the saved Wi-Fi networks over to Abyss: /data/misc/abyss/handoff/wifi.json, written right
 * before entering (while the Wi-Fi service still runs), read by abyssd (abyssd/src/wifi/
 * handoff.rs has the format), deleted by {@link BootReceiver} after the return because it holds
 * passphrases. Mode 0640 system:system (setgid dir), so abyssd (uid root, group system, no
 * CAP_DAC_OVERRIDE) can read it and no app can.
 *
 * <p>What goes in, per saved network (format v2): SSID (raw bytes as hex), hidden flag, every
 * security type Android has for it (WifiManager hands out one copy per type; {@link
 * WifiSecurity} merges them back by network id) plus the ones a transition-disable indication
 * turned off, the passphrase and WPA PMK (PBKDF2, so abyssd's supplicant skips 4096 rounds per
 * connect), SAE H2E/PK-only flags, WEP keys, the enterprise settings (method, phase 2,
 * identities, password, CA / client certificate references into the keystore, domain, OCSP,
 * minimum TLS), Easy Connect keys, a static IP configuration, Android's MAC policy and
 * per-network MAC, the last connection time, and the strongest BSSID + frequency + signal from
 * the last scan. Plus the network Android is on right now with its address (for DHCP
 * INIT-REBOOT), the country code, "Allow WEP networks" and Android's DHCP vendor class. abyssd
 * (abyssd/src/wifi/netblock.rs) decides what it can join and says why not on its Wi-Fi screen.
 *
 * <p>Hidden Wi-Fi APIs (SecurityParams, WifiEnterpriseConfig.getFieldValue, queryWepAllowed) are
 * read by reflection: this app is platform-signed, so the hidden-API checks let it, and every
 * one has a public/system-API fallback.
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
        root.put("v", 2);
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

        root.put("wep_allowed", wepAllowed(wm));
        boolean forceNonPersistent = false;
        try {
            forceNonPersistent = Settings.Global.getInt(c.getContentResolver(),
                    "non_persistent_mac_randomization_force_enabled", 0) == 1;
        } catch (RuntimeException e) {
            // Default off.
        }

        WifiInfo info = wm.getConnectionInfo();
        int currentId = info != null ? info.getNetworkId() : -1;
        JSONObject current = null;
        List<Integer> ids = new ArrayList<>();
        List<Integer> types = new ArrayList<>();
        List<Boolean> enabled = new ArrayList<>();
        for (WifiConfiguration w : configs) {
            ids.add(w.networkId);
            types.add(securityType(w));
            enabled.add(w.getNetworkSelectionStatus().getNetworkSelectionDisableReason()
                    != WifiConfiguration.NetworkSelectionStatus
                            .DISABLED_TRANSITION_DISABLE_INDICATION);
        }
        List<WifiSecurity.Merge<WifiConfiguration>> merged =
                WifiSecurity.merge(configs, ids, types, enabled);
        List<WifiSecurity.Merge<WifiConfiguration>> ordered = new ArrayList<>();
        for (WifiSecurity.Merge<WifiConfiguration> g : merged) {
            if (g.first.networkId == currentId) {
                ordered.add(0, g);
            } else {
                ordered.add(g);
            }
        }

        JSONArray nets = new JSONArray();
        int pmks = 0;
        for (WifiSecurity.Merge<WifiConfiguration> g : ordered) {
            if (nets.length() >= MAX_NETWORKS) {
                break;
            }
            WifiConfiguration w = g.first;
            byte[] ssid = ssidBytes(w.SSID);
            if (ssid == null || ssid.length == 0 || ssid.length > 32) {
                continue;
            }
            String hex = hex(ssid);
            JSONObject n = new JSONObject();
            n.put("key", String.valueOf(w.networkId));
            n.put("ssid", new String(ssid, StandardCharsets.UTF_8));
            n.put("ssid_hex", hex);
            n.put("hidden", w.hiddenSSID);
            n.put("security", new JSONArray(g.on));
            n.put("security_off", new JSONArray(g.off));
            boolean psk = g.on.contains("psk");
            String pre = w.preSharedKey;
            if (pre != null && pre.length() > 2 && pre.startsWith("\"") && pre.endsWith("\"")) {
                String pass = pre.substring(1, pre.length() - 1);
                n.put("passphrase", pass);
                if (psk && pmks < MAX_PMK && pass.length() >= 8 && pass.length() <= 63) {
                    try {
                        n.put("pmk", hex(pbkdf2Sha1(pass.getBytes(StandardCharsets.UTF_8), ssid, 4096, 32)));
                        pmks++;
                    } catch (GeneralSecurityException | RuntimeException e) {
                        Log.w(TAG, "wifi hand-over: PMK", e);
                    }
                }
            } else if (pre != null && pre.length() == 64) {
                // Stored as a raw PMK already (no passphrase known: WPA-PSK only).
                n.put("pmk", pre.toLowerCase(Locale.ROOT));
            }
            WifiConfiguration sae = g.byType.get("sae");
            if (sae != null) {
                n.put("sae_h2e_only", paramFlag(sae, "isSaeH2eOnlyMode"));
                n.put("sae_pk_only", paramFlag(sae, "isSaePkOnlyMode"));
            }
            if (g.byType.containsKey("wep") && w.wepKeys != null) {
                JSONArray keys = new JSONArray();
                for (int i = 0; i < 4; i++) {
                    String k = i < w.wepKeys.length ? w.wepKeys[i] : null;
                    keys.put(k != null ? k : JSONObject.NULL);
                }
                n.put("wep_keys", keys);
                n.put("wep_tx_keyidx", w.wepTxKeyIndex);
            }
            WifiConfiguration ent = g.byType.get("eap_suiteb");
            if (ent == null) ent = g.byType.get("eap_wpa3");
            if (ent == null) ent = g.byType.get("eap");
            if (ent != null && ent.enterpriseConfig != null) {
                n.put("eap", eap(ent.enterpriseConfig));
                JSONArray fils = new JSONArray();
                if (ent.allowedKeyManagement.get(WifiSecurity.KM_FILS_SHA256)) fils.put("sha256");
                if (ent.allowedKeyManagement.get(WifiSecurity.KM_FILS_SHA384)) fils.put("sha384");
                n.put("fils", fils);
            }
            WifiConfiguration suiteb = g.byType.get("eap_suiteb");
            if (suiteb != null) {
                JSONArray sb = new JSONArray();
                // WifiConfiguration.SuiteBCipher.ECDHE_ECDSA = 0, ECDHE_RSA = 1.
                if (suiteb.allowedSuiteBCiphers.get(1)) sb.put("rsa");
                if (suiteb.allowedSuiteBCiphers.get(0)) sb.put("ecdsa");
                n.put("suiteb", sb);
            }
            if (g.byType.containsKey("dpp")) {
                JSONObject d = new JSONObject();
                d.put("connector", new String(w.getDppConnector(), StandardCharsets.UTF_8));
                d.put("csign_hex", hex(w.getDppCSignKey()));
                d.put("netaccesskey_hex", hex(w.getDppNetAccessKey()));
                n.put("dpp", d);
            }
            boolean staticIp = addStaticIp(w, n);
            boolean open = g.on.contains("open") || g.on.contains("owe");
            WifiConfiguration.NetworkSelectionStatus ns = w.getNetworkSelectionStatus();
            String policy = WifiSecurity.macPolicy(w.macRandomizationSetting, open, staticIp,
                    ns.hasEverConnected(), neverCaptive(ns),
                    // config_wifiAllowNonPersistentMacRandomizationOnOpenSsids (device overlay).
                    true, forceNonPersistent);
            n.put("mac_policy", policy);
            if (!"factory".equals(policy)) {
                n.put("mac", w.getRandomizedMacAddress().toString());
            }
            n.put("last_connected_ms", w.lastConnected);
            ScanResult r = best.get(hex);
            if (r != null) {
                n.put("bssid", r.BSSID);
                n.put("freq", r.frequency);
                n.put("rssi", r.level);
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
        root.put("passpoint_profiles", passpointProfiles(wm, configs));
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

    /**
     * The security type of one per-type copy: SecurityParams.getSecurityType() (hidden), else
     * the legacy bit sets.
     */
    private static int securityType(WifiConfiguration w) {
        Object p = defaultParams(w);
        if (p != null) {
            try {
                Object t = p.getClass().getMethod("getSecurityType").invoke(p);
                if (t instanceof Integer) {
                    return (Integer) t;
                }
            } catch (ReflectiveOperationException | RuntimeException e) {
                // Fall back below.
            }
        }
        return WifiSecurity.fromLegacy(w.allowedKeyManagement, w.allowedProtocols,
                w.allowedPairwiseCiphers, w.allowedGroupCiphers, w.requirePmf,
                w.wepKeys != null && w.wepKeys[0] != null);
    }

    private static Object defaultParams(WifiConfiguration w) {
        try {
            return WifiConfiguration.class.getMethod("getDefaultSecurityParams").invoke(w);
        } catch (ReflectiveOperationException | RuntimeException e) {
            return null;
        }
    }

    /** A boolean getter of the copy's SecurityParams (false when it cannot be read). */
    private static boolean paramFlag(WifiConfiguration w, String getter) {
        Object p = defaultParams(w);
        if (p == null) {
            return false;
        }
        try {
            return Boolean.TRUE.equals(p.getClass().getMethod(getter).invoke(p));
        } catch (ReflectiveOperationException | RuntimeException e) {
            return false;
        }
    }

    private static boolean neverCaptive(WifiConfiguration.NetworkSelectionStatus ns) {
        try {
            Method m = ns.getClass().getMethod("hasNeverDetectedCaptivePortal");
            return Boolean.TRUE.equals(m.invoke(ns));
        } catch (ReflectiveOperationException | RuntimeException e) {
            return false;
        }
    }

    /** Android's "Allow WEP networks" (Android 15+); true when it cannot be asked. */
    private static boolean wepAllowed(WifiManager wm) {
        try {
            Method m = WifiManager.class.getMethod("queryWepAllowed",
                    java.util.concurrent.Executor.class, java.util.function.Consumer.class);
            CountDownLatch done = new CountDownLatch(1);
            AtomicReference<Boolean> out = new AtomicReference<>(Boolean.TRUE);
            java.util.function.Consumer<Boolean> cb = v -> {
                out.set(v);
                done.countDown();
            };
            m.invoke(wm, (java.util.concurrent.Executor) Runnable::run, cb);
            done.await(500, TimeUnit.MILLISECONDS);
            return !Boolean.FALSE.equals(out.get());
        } catch (ReflectiveOperationException | RuntimeException e) {
            return true;
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return true;
        }
    }

    /** A supplicant field as Android stores it (WifiEnterpriseConfig.getFieldValue, hidden). */
    private static String field(WifiEnterpriseConfig ec, String key) {
        try {
            Object v = WifiEnterpriseConfig.class.getMethod("getFieldValue", String.class)
                    .invoke(ec, key);
            return v instanceof String && !((String) v).isEmpty() ? (String) v : null;
        } catch (ReflectiveOperationException | RuntimeException e) {
            return null;
        }
    }

    private static void putIf(JSONObject o, String k, String v) throws JSONException {
        if (v != null && !v.isEmpty()) {
            o.put(k, v);
        }
    }

    /** The enterprise settings the Wi-Fi HAL would give the supplicant. */
    private static JSONObject eap(WifiEnterpriseConfig ec) throws JSONException {
        JSONObject e = new JSONObject();
        String method = WifiSecurity.eapMethod(ec.getEapMethod());
        putIf(e, "method", method);
        e.put("phase2", WifiSecurity.phase2(ec.getPhase2Method()));
        putIf(e, "identity", ec.getIdentity());
        String anon = ec.getAnonymousIdentity();
        String prefix = field(ec, "decorated_username_prefix");
        if (anon != null && !anon.isEmpty() && prefix != null) {
            anon = prefix + anon;
        }
        putIf(e, "anonymous_identity", anon);
        putIf(e, "password", ec.getPassword());
        String ca = field(ec, "ca_cert");
        if (ca == null) {
            try {
                ca = WifiSecurity.caCertField(ec.getCaCertificateAliases());
            } catch (RuntimeException ex) {
                ca = null;
            }
        }
        putIf(e, "ca_cert", ca);
        putIf(e, "ca_path", ec.getCaPath());
        String alias = ec.getClientCertificateAlias();
        boolean hasAlias = alias != null && !alias.isEmpty();
        String cc = field(ec, "client_cert");
        putIf(e, "client_cert", cc != null ? cc : hasAlias ? "keystore://USRCERT_" + alias : null);
        String key = field(ec, "key_id");
        putIf(e, "key_id", key != null ? key : hasAlias ? "USRPKEY_" + alias : null);
        String engine = field(ec, "engine");
        e.put("engine", engine != null ? "1".equals(engine) : hasAlias);
        String engineId = field(ec, "engine_id");
        putIf(e, "engine_id", engineId != null ? engineId : hasAlias ? "keystore" : null);
        putIf(e, "domain_suffix_match", ec.getDomainSuffixMatch());
        putIf(e, "altsubject_match", ec.getAltSubjectMatch());
        putIf(e, "subject_match", field(ec, "subject_match"));
        e.put("ocsp", ec.getOcsp());
        e.put("erp", "1".equals(field(ec, "eap_erp")));
        e.put("min_tls", ec.getMinimumTlsVersion());
        return e;
    }

    /**
     * Names of the Passpoint profiles that are not among the saved networks (Android makes a
     * network of a profile only once it matched an AP). abyssd lists them as not joinable.
     */
    private static JSONArray passpointProfiles(WifiManager wm, List<WifiConfiguration> configs) {
        JSONArray out = new JSONArray();
        try {
            List<android.net.wifi.hotspot2.PasspointConfiguration> ps =
                    wm.getPasspointConfigurations();
            if (ps == null) {
                return out;
            }
            for (android.net.wifi.hotspot2.PasspointConfiguration p : ps) {
                if (p.getHomeSp() == null) {
                    continue;
                }
                String fqdn = p.getHomeSp().getFqdn();
                boolean saved = false;
                for (WifiConfiguration w : configs) {
                    if (w.isPasspoint() && fqdn != null && fqdn.equals(w.FQDN)) {
                        saved = true;
                        break;
                    }
                }
                if (!saved) {
                    String name = p.getHomeSp().getFriendlyName();
                    out.put(name != null && !name.isEmpty() ? name : String.valueOf(fqdn));
                }
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "wifi hand-over: no Passpoint profiles", e);
        }
        return out;
    }

    /** Android's static IPv4 settings for the network, if it has some; returns whether. */
    private static boolean addStaticIp(WifiConfiguration w, JSONObject n) throws JSONException {
        IpConfiguration ipc = w.getIpConfiguration();
        if (ipc == null || ipc.getIpAssignment() != IpConfiguration.IpAssignment.STATIC) {
            return false;
        }
        StaticIpConfiguration st = ipc.getStaticIpConfiguration();
        if (st == null || st.getIpAddress() == null
                || !(st.getIpAddress().getAddress() instanceof Inet4Address)) {
            return true;
        }
        JSONObject o = new JSONObject();
        o.put("ip", st.getIpAddress().getAddress().getHostAddress());
        o.put("prefix", st.getIpAddress().getPrefixLength());
        if (st.getGateway() instanceof Inet4Address) {
            o.put("gateway", st.getGateway().getHostAddress());
        }
        JSONArray dns = new JSONArray();
        for (InetAddress a : st.getDnsServers()) {
            if (a instanceof Inet4Address) {
                dns.put(a.getHostAddress());
            }
        }
        o.put("dns", dns);
        n.put("static_ip", o);
        return true;
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
