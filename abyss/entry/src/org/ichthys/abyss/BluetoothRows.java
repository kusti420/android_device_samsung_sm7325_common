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

import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothClass;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothManager;
import android.content.Intent;
import android.os.ParcelUuid;
import android.provider.Settings;
import android.text.TextUtils;
import android.util.Log;
import android.view.Gravity;
import android.view.ViewGroup;
import android.widget.LinearLayout;
import android.widget.TextView;

import org.json.JSONObject;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/**
 * Playback & Bluetooth › Bluetooth headphones: Android's paired audio devices. abyss-btd runs
 * the same stack on the same bonding store (/data/misc/bluedroid), so this is the list Abyss
 * offers. "Last used" is abyssd's out/bt_last.json; it is shown even while Android's Bluetooth
 * is off.
 */
final class BluetoothRows {
    private static final String TAG = "AbyssEntry";
    /** abyssd's last Bluetooth headphones: {"addr": "...", "name": "..."} (bt.rs LAST_FILE). */
    private static final String BT_LAST_FILE = Captures.DIR + "/bt_last.json";
    /** A2DP sink (headphones / speaker) service class UUID. */
    private static final ParcelUuid A2DP_SINK =
            ParcelUuid.fromString("0000110B-0000-1000-8000-00805F9B34FB");

    private final Page mPage;
    private LinearLayout mList;

    BluetoothRows(Page p) {
        mPage = p;
    }

    void build(Page.Group g) {
        LinearLayout col = new LinearLayout(mPage.a);
        col.setOrientation(LinearLayout.VERTICAL);
        col.addView(mPage.summary(mPage.a.getString(R.string.bt_summary)));
        mList = new LinearLayout(mPage.a);
        mList.setOrientation(LinearLayout.VERTICAL);
        col.addView(mList, mPage.lp(8));
        TextView pair = mPage.text(mPage.a.getString(R.string.bt_open_settings), 15,
                R.color.abyss_text, mPage.bodyFace(true));
        pair.setGravity(Gravity.CENTER);
        pair.setMinHeight(mPage.dp(44));
        pair.setPadding(mPage.dp(20), 0, mPage.dp(20), 0);
        pair.setBackground(mPage.outlineDrawable());
        pair.setClickable(true);
        pair.setFocusable(true);
        pair.setOnClickListener(v -> mPage.start(new Intent(Settings.ACTION_BLUETOOTH_SETTINGS)));
        LinearLayout.LayoutParams l = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        l.topMargin = mPage.dp(12);
        col.addView(pair, l);
        g.view(col);
        mPage.onRefresh(this::refresh);
    }

    private void refresh() {
        mList.removeAllViews();
        String[] last = readLast();
        boolean lastShown = false;
        BluetoothManager bm = mPage.a.getSystemService(BluetoothManager.class);
        BluetoothAdapter adapter = bm == null ? null : bm.getAdapter();
        boolean on = false;
        List<BluetoothDevice> audio = new ArrayList<>();
        try {
            on = adapter != null && adapter.isEnabled();
            if (on) {
                Set<BluetoothDevice> bonded = adapter.getBondedDevices();
                if (bonded != null) {
                    for (BluetoothDevice d : bonded) {
                        if (isAudio(d)) {
                            audio.add(d);
                        }
                    }
                }
            }
        } catch (SecurityException e) {
            // BLUETOOTH_CONNECT: granted to the system uid; logged in case that ever changes.
            Log.w(TAG, "bonded devices", e);
        }
        for (BluetoothDevice d : audio) {
            boolean isLast = last != null && last[0].equalsIgnoreCase(d.getAddress());
            lastShown |= isLast;
            mList.addView(row(nameOf(d), isLast));
        }
        if (last != null && !lastShown) {
            mList.addView(row(last[1].isEmpty() ? last[0] : last[1], true));
        }
        if (audio.isEmpty()) {
            TextView t = mPage.summary(mPage.a.getString(on ? R.string.bt_none : R.string.bt_off));
            t.setPadding(0, mPage.dp(6), 0, 0);
            mList.addView(t);
        }
    }

    private static boolean isAudio(BluetoothDevice d) {
        ParcelUuid[] uuids = d.getUuids();
        if (uuids != null) {
            for (ParcelUuid u : uuids) {
                if (A2DP_SINK.equals(u)) {
                    return true;
                }
            }
        }
        BluetoothClass c = d.getBluetoothClass();
        return c != null && (c.getMajorDeviceClass() == BluetoothClass.Device.Major.AUDIO_VIDEO
                || c.hasService(BluetoothClass.Service.RENDER)
                        && c.hasService(BluetoothClass.Service.AUDIO));
    }

    private static String nameOf(BluetoothDevice d) {
        String n = d.getAlias();
        if (TextUtils.isEmpty(n)) {
            n = d.getName();
        }
        return TextUtils.isEmpty(n) ? d.getAddress() : n;
    }

    private LinearLayout row(String name, boolean last) {
        LinearLayout row = new LinearLayout(mPage.a);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER_VERTICAL);
        row.setPadding(0, mPage.dp(8), 0, mPage.dp(8));
        TextView n = mPage.text(name, 16, R.color.abyss_text, mPage.bodyFace(false));
        row.addView(n, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
        if (last) {
            row.addView(mPage.text(mPage.a.getString(R.string.bt_last_used), 13,
                    R.color.abyss_accent, mPage.bodyFace(true)));
        }
        return row;
    }

    /** The headphones Abyss used last, or null. */
    static String lastName() {
        String[] l = readLast();
        return l == null ? null : l[1].isEmpty() ? l[0] : l[1];
    }

    /** {addr, name} from out/bt_last.json, or null. */
    static String[] readLast() {
        try {
            String text = new String(Files.readAllBytes(new File(BT_LAST_FILE).toPath()),
                    StandardCharsets.UTF_8);
            JSONObject o = new JSONObject(text);
            String addr = o.optString("addr", "").trim();
            if (addr.isEmpty()) {
                return null;
            }
            return new String[] {addr.toUpperCase(Locale.ROOT), o.optString("name", "").trim()};
        } catch (java.nio.file.NoSuchFileException e) {
            return null;
        } catch (Exception e) {
            Log.w(TAG, "read " + BT_LAST_FILE, e);
            return null;
        }
    }
}
