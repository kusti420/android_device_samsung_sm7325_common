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

import android.content.ContentResolver;
import android.content.Context;
import android.database.Cursor;
import android.net.Uri;
import android.provider.Telephony;
import android.system.ErrnoException;
import android.system.Os;
import android.telephony.SubscriptionManager;
import android.telephony.TelephonyManager;
import android.text.TextUtils;
import android.util.Log;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Locale;

/**
 * Hands the SIM's mobile-data settings over to Abyss: /data/misc/abyss/handoff/mobile.json,
 * written right before entering (while telephony still runs; GoDark calls it before airplane
 * mode goes on), read by abyssd (abyssd/src/mobile/handoff.rs has the format), deleted by
 * {@link BootReceiver} after the return (an APN may carry a password). Mode 0640 system:system
 * (setgid dir), like the Wi-Fi hand-over.
 *
 * <p>What goes in: the default data subscription's slot, the network operator (name +
 * MCC/MNC), whether Android's mobile data and data roaming are on, the SIM state, and the APN
 * Android uses for internet: the SIM's preferred APN, else the first enabled APN of the SIM's
 * carrier that serves "default". The system uid passes TelephonyProvider's WRITE_APN_SETTINGS
 * check by uid, so no new privileged permission is needed (an unlisted one would crash-loop
 * system_server in privapp enforce mode).
 */
final class MobileHandoff {
    private static final String TAG = "AbyssEntry";
    static final String FILE = "/data/misc/abyss/handoff/mobile.json";

    private MobileHandoff() {}

    static void write(Context c) {
        try {
            JSONObject root = build(c);
            if (root == null) {
                clear();
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
            JSONObject apn = root.optJSONObject("apn");
            Log.i(TAG, "mobile hand-over: " + root.optString("operator") + " slot "
                    + root.optInt("slot") + ", data " + (root.optBoolean("data_enabled") ? "on" : "off")
                    + ", apn " + (apn == null ? "none" : apn.optString("apn")));
        } catch (IOException | JSONException | ErrnoException | RuntimeException e) {
            Log.w(TAG, "mobile hand-over failed; Abyss will have no mobile data", e);
        }
    }

    /** After the return. */
    static void clear() {
        File f = new File(FILE);
        if (f.exists() && !f.delete()) {
            Log.w(TAG, "could not delete " + FILE);
        }
    }

    private static JSONObject build(Context c) throws JSONException {
        int sub = SubscriptionManager.getDefaultDataSubscriptionId();
        if (!SubscriptionManager.isValidSubscriptionId(sub)) {
            Log.i(TAG, "mobile hand-over: no data SIM");
            return null;
        }
        TelephonyManager base = c.getSystemService(TelephonyManager.class);
        if (base == null) {
            return null;
        }
        TelephonyManager tm = base.createForSubscriptionId(sub);
        String simNumeric = nz(tm.getSimOperator());
        String netNumeric = nz(tm.getNetworkOperator());
        String netName = nz(tm.getNetworkOperatorName());
        JSONObject root = new JSONObject();
        root.put("v", 1);
        root.put("written_ms", System.currentTimeMillis());
        root.put("slot", SubscriptionManager.getSlotIndex(sub));
        root.put("sub_id", sub);
        root.put("mcc_mnc", netNumeric.isEmpty() ? simNumeric : netNumeric);
        root.put("operator", netName.isEmpty() ? nz(tm.getSimOperatorName()) : netName);
        root.put("data_enabled", tm.isDataEnabled());
        root.put("roaming_allowed", tm.isDataRoamingEnabled());
        root.put("sim_state", simState(tm.getSimState()));
        JSONObject apn = apn(c.getContentResolver(), sub, simNumeric);
        if (apn != null) {
            root.put("apn", apn);
        }
        return root;
    }

    private static String nz(String s) {
        return s == null ? "" : s.trim();
    }

    private static String simState(int s) {
        switch (s) {
            case TelephonyManager.SIM_STATE_ABSENT: return "ABSENT";
            case TelephonyManager.SIM_STATE_PIN_REQUIRED: return "PIN_REQUIRED";
            case TelephonyManager.SIM_STATE_PUK_REQUIRED: return "PUK_REQUIRED";
            case TelephonyManager.SIM_STATE_NETWORK_LOCKED: return "NETWORK_LOCKED";
            case TelephonyManager.SIM_STATE_READY: return "READY";
            case TelephonyManager.SIM_STATE_NOT_READY: return "NOT_READY";
            case TelephonyManager.SIM_STATE_PERM_DISABLED: return "PERM_DISABLED";
            case TelephonyManager.SIM_STATE_CARD_IO_ERROR: return "CARD_IO_ERROR";
            case TelephonyManager.SIM_STATE_CARD_RESTRICTED: return "CARD_RESTRICTED";
            case TelephonyManager.SIM_STATE_LOADED: return "READY";
            default: return "UNKNOWN";
        }
    }

    private static final String[] COLUMNS = {
        Telephony.Carriers.APN, Telephony.Carriers.USER, Telephony.Carriers.PASSWORD,
        Telephony.Carriers.AUTH_TYPE, Telephony.Carriers.PROTOCOL,
        Telephony.Carriers.ROAMING_PROTOCOL, Telephony.Carriers.TYPE,
        Telephony.Carriers.CARRIER_ENABLED,
    };

    /** Does an APN with these types carry internet traffic? (ApnSetting: empty or "*" = all). */
    static boolean servesDefault(String types) {
        if (TextUtils.isEmpty(types)) {
            return true;
        }
        for (String t : types.toLowerCase(Locale.ROOT).split(",")) {
            String x = t.trim();
            if (x.equals("default") || x.equals("*")) {
                return true;
            }
        }
        return false;
    }

    private static JSONObject apn(ContentResolver cr, int sub, String simNumeric) throws JSONException {
        // 1. The APN Android picked for this SIM (Settings › Access point names).
        JSONObject pref = first(cr, Uri.parse("content://telephony/carriers/preferapn/subId/" + sub), null, null);
        if (pref != null) {
            return pref;
        }
        // 2. The carrier's own list: first enabled APN that serves "default".
        if (simNumeric.isEmpty()) {
            return null;
        }
        return first(cr, Telephony.Carriers.CONTENT_URI, Telephony.Carriers.NUMERIC + " = ? AND "
                + Telephony.Carriers.CARRIER_ENABLED + " = 1", new String[] {simNumeric});
    }

    private static JSONObject first(ContentResolver cr, Uri uri, String sel, String[] args)
            throws JSONException {
        try (Cursor cur = cr.query(uri, COLUMNS, sel, args, null)) {
            if (cur == null) {
                return null;
            }
            while (cur.moveToNext()) {
                String type = cur.getString(6);
                int enabledIdx = 7;
                if (!cur.isNull(enabledIdx) && cur.getInt(enabledIdx) == 0) {
                    continue;
                }
                if (!servesDefault(type)) {
                    continue;
                }
                JSONObject o = new JSONObject();
                o.put("apn", nz(cur.getString(0)));
                o.put("user", nz(cur.getString(1)));
                o.put("password", nz(cur.getString(2)));
                o.put("auth", cur.isNull(3) ? -1 : cur.getInt(3));
                o.put("protocol", nz(cur.getString(4)));
                o.put("roaming_protocol", nz(cur.getString(5)));
                o.put("types", nz(type));
                if (o.getString("apn").isEmpty()) {
                    continue;
                }
                return o;
            }
        } catch (SecurityException | IllegalArgumentException e) {
            Log.w(TAG, "mobile hand-over: cannot read APNs from " + uri, e);
        }
        return null;
    }
}
