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
import android.content.SharedPreferences;
import android.net.ConnectivityManager;
import android.net.wifi.WifiManager;
import android.os.SystemClock;
import android.provider.Settings;
import android.util.Log;

/**
 * What the main OS switches off right before entering Abyss, and puts back after the return.
 *
 * <p>Done here (not in abyss.rc) because these are framework settings: system_server must still
 * be running to apply them. Saved in device-protected storage so {@link BootReceiver} can restore
 * them at LOCKED_BOOT_COMPLETED after Android comes back.
 *
 * <ul>
 * <li>Wi-Fi hand-over first ({@link WifiHandoff}): the saved networks, the current BSSID/channel
 *     and address, while the Wi-Fi service still has them.</li>
 * <li>Airplane mode on: modem, Wi-Fi and Bluetooth radios off. Airplane mode is a framework
 *     setting only: it blocks nothing in the kernel (no rfkill on the QCA WLAN driver), so abyssd
 *     can still bring Wi-Fi up on its own for a lease while the modem stays off.</li>
 * <li>Wi-Fi really off before entering: if the user lets Wi-Fi stay on in airplane mode, it is
 *     switched off here (and back on after the return), and we wait until it is down, so
 *     ConnectivityService has removed its network from netd before system_server stops (a left
 *     over network would hold wlan0 and break both Abyss and Android Wi-Fi after the return).</li>
 * <li>Double-tap-to-wake off: otherwise the touch controller keeps scanning with the screen off.</li>
 * </ul>
 */
final class GoDark {
    private static final String TAG = "AbyssEntry";
    private static final String PREFS = "go_dark";
    private static final String K_SAVED = "saved";
    private static final String K_AIRPLANE = "airplane";
    private static final String K_DT2W = "dt2w";
    /** We switched Wi-Fi off ourselves (it stayed on in airplane mode): switch it back on. */
    private static final String K_WIFI = "wifi_forced_off";
    /** Waits for Wi-Fi to reach DISABLED (UI thread; the whole enter tap stays well under 5 s). */
    private static final long WIFI_AIRPLANE_WAIT_MS = 1500;
    private static final long WIFI_OFF_WAIT_MS = 2000;
    /** ConnectivityService tears the network down right after the Wi-Fi state change. */
    private static final long NETWORK_TEARDOWN_MS = 300;
    private static final String DT2W = "double_tap_to_wake";

    private GoDark() {}

    private static SharedPreferences prefs(Context c) {
        return c.createDeviceProtectedStorageContext().getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    /** Save the current state and switch everything off. Never throws. */
    static void apply(Context c) {
        try {
            boolean airplane = Settings.Global.getInt(
                    c.getContentResolver(), Settings.Global.AIRPLANE_MODE_ON, 0) == 1;
            int dt2w = Settings.Secure.getInt(c.getContentResolver(), DT2W, 0);
            prefs(c).edit()
                    .putBoolean(K_SAVED, true)
                    .putBoolean(K_AIRPLANE, airplane)
                    .putInt(K_DT2W, dt2w)
                    .putBoolean(K_WIFI, false)
                    .commit();
            WifiHandoff.write(c);
            if (!airplane) {
                c.getSystemService(ConnectivityManager.class).setAirplaneMode(true);
            }
            if (dt2w != 0) {
                Settings.Secure.putInt(c.getContentResolver(), DT2W, 0);
            }
            boolean forced = wifiOff(c);
            if (forced) {
                prefs(c).edit().putBoolean(K_WIFI, true).commit();
            }
            Log.i(TAG, "go dark: airplane on (was " + airplane + "), double-tap wake off (was " + dt2w
                    + "), wifi switched off by us: " + forced);
        } catch (RuntimeException e) {
            Log.w(TAG, "go dark failed; entering anyway", e);
        }
    }

    /**
     * Waits until Wi-Fi is DISABLED. If airplane mode leaves it on (the user's "keep Wi-Fi on"
     * choice), switches it off. Returns true if we switched it off (restore turns it back on).
     */
    private static boolean wifiOff(Context c) {
        WifiManager wm = c.getSystemService(WifiManager.class);
        if (wm == null || wm.getWifiState() == WifiManager.WIFI_STATE_DISABLED) {
            return false;
        }
        boolean forced = false;
        if (!waitDisabled(wm, WIFI_AIRPLANE_WAIT_MS)) {
            Log.i(TAG, "go dark: Wi-Fi stays on in airplane mode; switching it off");
            forced = wm.setWifiEnabled(false);
            if (!waitDisabled(wm, WIFI_OFF_WAIT_MS)) {
                Log.w(TAG, "go dark: Wi-Fi still not off (state " + wm.getWifiState() + "); entering anyway");
                return forced;
            }
        }
        SystemClock.sleep(NETWORK_TEARDOWN_MS);
        return forced;
    }

    private static boolean waitDisabled(WifiManager wm, long ms) {
        long end = SystemClock.uptimeMillis() + ms;
        while (SystemClock.uptimeMillis() < end) {
            if (wm.getWifiState() == WifiManager.WIFI_STATE_DISABLED) {
                return true;
            }
            SystemClock.sleep(100);
        }
        return wm.getWifiState() == WifiManager.WIFI_STATE_DISABLED;
    }

    /** Put back what {@link #apply} changed, once. Never throws. */
    static void restore(Context c) {
        SharedPreferences p = prefs(c);
        if (!p.getBoolean(K_SAVED, false)) {
            return;
        }
        try {
            if (!p.getBoolean(K_AIRPLANE, false)) {
                c.getSystemService(ConnectivityManager.class).setAirplaneMode(false);
            }
            if (p.getBoolean(K_WIFI, false)) {
                c.getSystemService(WifiManager.class).setWifiEnabled(true);
            }
            int dt2w = p.getInt(K_DT2W, 0);
            if (dt2w != 0) {
                Settings.Secure.putInt(c.getContentResolver(), DT2W, dt2w);
            }
            Log.i(TAG, "back from Abyss: radios and double-tap wake restored");
        } catch (RuntimeException e) {
            Log.w(TAG, "restore after Abyss failed", e);
        } finally {
            p.edit().clear().commit();
        }
    }
}
