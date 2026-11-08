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
import android.media.AudioDeviceInfo;
import android.media.AudioManager;
import android.os.BatteryManager;
import android.os.SystemProperties;
import android.util.Log;

import org.json.JSONObject;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.NoSuchFileException;
import java.util.Locale;

/**
 * The top page's state lines: what Abyss will play, where the sound goes, and the battery.
 * Read from abyssd's own files in /data/misc/abyss/out (written only by abyssd; this app only
 * reads them): state.json (library.rs ResumeState), last_stream.json (modules.rs),
 * bt_last.json (bt.rs) and screen_off_draw (battery.rs OffDraw: "&lt;mA&gt; [cc]").
 */
final class Status {
    private static final String TAG = "AbyssEntry";
    private static final String STATE = Captures.DIR + "/state.json";
    private static final String LAST_STREAM = Captures.DIR + "/last_stream.json";
    private static final String OFF_DRAW = Captures.DIR + "/screen_off_draw";
    /** battery.rs: remaining = capacity% x 4500 mAh x 0.97 (3 % reserve) / draw. */
    private static final double DESIGN_MAH = 4500.0;
    private static final double RESERVE = 0.97;

    private Status() {
    }

    /** What the first ● plays (persist.abyss.nav.start + abyssd's resume state). */
    static String plays(Context c) {
        if (Sections.NAV_START.resolve(SystemProperties.get(Sections.NAV_START.prop, "")) == 1) {
            return c.getString(R.string.state_plays_home);
        }
        JSONObject st = readJson(STATE);
        if (st != null && "stream".equals(st.optString("source"))) {
            JSONObject ls = readJson(LAST_STREAM);
            String title = ls == null ? "" : ls.optString("title", "").trim();
            if (!title.isEmpty()) {
                return c.getString(R.string.state_plays_radio, title);
            }
        }
        String track = st == null ? "" : st.optString("track", "").trim();
        if (!track.isEmpty()) {
            return c.getString(R.string.state_plays_song, songName(track));
        }
        return c.getString(R.string.state_plays_nothing);
    }

    /** "4782. Artist - Title.flac" -> "Artist - Title". */
    static String songName(String path) {
        String n = new File(path).getName();
        int dot = n.lastIndexOf('.');
        if (dot > 0) {
            n = n.substring(0, dot);
        }
        return n.replaceFirst("^\\d+\\s*[.)_-]\\s*", "").trim();
    }

    /** Wired or USB headphones plugged in now. */
    static boolean wired(AudioManager am) {
        for (AudioDeviceInfo d : am.getDevices(AudioManager.GET_DEVICES_OUTPUTS)) {
            int t = d.getType();
            if (t == AudioDeviceInfo.TYPE_WIRED_HEADPHONES
                    || t == AudioDeviceInfo.TYPE_WIRED_HEADSET
                    || t == AudioDeviceInfo.TYPE_USB_HEADSET
                    || t == AudioDeviceInfo.TYPE_USB_DEVICE) {
                return true;
            }
        }
        return false;
    }

    /** The output line; null when there is nothing to play on (the warning shows instead). */
    static String output(Context c, AudioManager am) {
        if (wired(am)) {
            return c.getString(R.string.state_out_wired);
        }
        String bt = BluetoothRows.lastName();
        return bt == null ? null : c.getString(R.string.state_out_bt, bt);
    }

    /** "78 % · about 31 h of music" (the hours once abyssd has learned the screen-off draw). */
    static String battery(Context c) {
        int pct = capacity(c);
        if (pct < 0) {
            return c.getString(R.string.state_battery_unknown);
        }
        double ma = offDraw();
        if (ma <= 0) {
            return c.getString(R.string.state_battery, pct);
        }
        return c.getString(R.string.state_battery_hours, pct, hours(pct, ma));
    }

    /** Screen & power › Battery: the same numbers with how they are made. */
    static String batteryDetail(Context c) {
        int pct = capacity(c);
        double ma = offDraw();
        if (pct < 0) {
            return c.getString(R.string.state_battery_unknown);
        }
        if (ma <= 0) {
            return c.getString(R.string.battery_detail_unlearned, pct);
        }
        return c.getString(R.string.battery_detail, pct, Math.round(ma), hours(pct, ma));
    }

    private static String hours(int pct, double ma) {
        double h = pct / 100.0 * DESIGN_MAH * RESERVE / ma;
        return h >= 10 ? Long.toString(Math.round(h)) : String.format(Locale.ROOT, "%.1f", h);
    }

    private static int capacity(Context c) {
        BatteryManager bm = c.getSystemService(BatteryManager.class);
        int v = bm == null ? -1 : bm.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY);
        return v < 0 || v > 100 ? -1 : v;
    }

    /** The learned screen-off music draw in mA, or 0 when abyssd has not measured it yet. */
    private static double offDraw() {
        try {
            String t = new String(Files.readAllBytes(new File(OFF_DRAW).toPath()),
                    StandardCharsets.UTF_8).trim();
            double v = Double.parseDouble(t.split("\\s+")[0]);
            return v >= 1 && v < 2000 ? v : 0;
        } catch (NoSuchFileException e) {
            return 0;
        } catch (Exception e) {
            Log.w(TAG, "read " + OFF_DRAW, e);
            return 0;
        }
    }

    private static JSONObject readJson(String path) {
        try {
            return new JSONObject(new String(Files.readAllBytes(new File(path).toPath()),
                    StandardCharsets.UTF_8));
        } catch (NoSuchFileException e) {
            return null;
        } catch (Exception e) {
            Log.w(TAG, "read " + path, e);
            return null;
        }
    }
}
