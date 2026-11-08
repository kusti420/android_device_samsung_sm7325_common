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
import android.content.Intent;
import android.os.SystemProperties;
import android.text.TextUtils;

import java.io.File;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;
import java.util.function.Function;

/**
 * WHERE EVERY ROW OF SETTINGS › ABYSS LIVES. The top page ({@link EntryActivity}) shows Enter,
 * the state lines, {@link #quick} and the list of {@link #ALL} sections; each section opens in
 * {@link SectionActivity} and is built by its method below. Rows are drawn by {@link Page}.
 *
 * <p>To add a setting:
 * <ol>
 * <li>Declare the property here (a {@code PROP_*} string, or a {@link Page.Choice} for a fixed
 *     set of values: first argument the property, then values, label strings, default).</li>
 * <li>Add one row in the section's method, e.g.
 *     {@code g.choice(R.string.x_title, R.string.x_summary, X);} or
 *     {@code g.toggle(R.string.x_title, R.string.x_summary, PROP_X, false, false);}.
 *     Rows re-read their property on every onResume; nothing else needs wiring.</li>
 * <li>If people change it often, also call the same row from {@link #quick} (the top page);
 *     if the section's one-line summary should mention it, extend the section's
 *     {@code *Summary} method.</li>
 * <li>Strings go in res/values/strings_sections.xml (new) or strings.xml (older rows).</li>
 * </ol>
 * A new section: a method {@code static void foo(Page p)}, a summary, and one {@link Section}
 * line in {@link #ALL} (SectionActivity finds it by id). A section that is its own activity
 * (Modules) passes the activity class instead of a builder.
 *
 * <p>Every property is persist.abyss.* (abyss_config_prop, abyss_entry.te); abyssd and
 * abyss-btd read them on entry / when Bluetooth starts in Abyss. Protocol:
 * device/samsung/sm7325-common/abyss/INTEGRATION.md; abyssd's list: abyss/abyssd/README.md.
 */
final class Sections {
    private Sections() {
    }

    // ---- Properties ------------------------------------------------------------------------

    /** Backlight level abyssd asks for (raw panel0-backlight units, max 486 on a52sxq). */
    static final String PROP_BRIGHTNESS = "persist.abyss.brightness";
    static final int BRIGHTNESS_MIN = 1;
    static final int BRIGHTNESS_MAX = 486;
    static final int BRIGHTNESS_DEFAULT = 160;

    /** Seconds the screen stays on after a key press (abyssd clamps to 5..120). */
    static final Page.Choice SCREEN_TIMEOUT = Page.Choice.ints("persist.abyss.screen_timeout_s",
            new int[] {10, 15, 30, 60}, R.string.seconds, 15, 5, 120,
            R.string.screen_timeout_other);

    /** Where Abyss opens: "player" (default: last song ready, paused) or "home" (abyssd ui.rs). */
    static final Page.Choice NAV_START = new Page.Choice("persist.abyss.nav.start",
            new String[] {"player", "home"},
            new int[] {R.string.nav_start_player, R.string.nav_start_home}, "player");

    /** Best A2DP codec abyss-btd may use; the stack falls back to AAC, then SBC (daemon.rs). */
    static final Page.Choice BT_CODEC = new Page.Choice("persist.abyss.bt.codec",
            new String[] {"ldac", "aac", "sbc"},
            new int[] {R.string.bt_codec_ldac, R.string.bt_codec_aac, R.string.bt_codec_sbc},
            "ldac");

    /** LDAC bitrate for Bluetooth in Abyss (abyss-btd daemon.rs). */
    static final Page.Choice BT_LDAC_QUALITY = new Page.Choice("persist.abyss.bt.ldac_quality",
            new String[] {"990", "660", "330", "abr"},
            new int[] {R.string.bt_quality_990, R.string.bt_quality_660,
                    R.string.bt_quality_330, R.string.bt_quality_abr}, "990");

    /** A2DP through the audio DSP (abyss-btd + abyssd a2dp_dsp.rs); opt-in, "1" = on. */
    static final String PROP_BT_OFFLOAD = "persist.abyss.bt.offload";

    /** Internet in Abyss (abyssd uplink.rs). */
    static final Page.Choice NET_MODE = new Page.Choice("persist.abyss.net.mode",
            new String[] {"wifi_mobile", "wifi", "mobile"},
            new int[] {R.string.net_mode_wifi_mobile, R.string.net_mode_wifi,
                    R.string.net_mode_mobile}, "wifi_mobile");

    /** Radio stream format on mobile data (abyssd uplink.rs). */
    static final Page.Choice NET_FORMAT = new Page.Choice("persist.abyss.net.mobile_format",
            new String[] {"same", "mp3-128"},
            new int[] {R.string.net_format_same, R.string.net_format_small}, "same");

    /** Mobile network while Abyss uses mobile data: "lte" (default) or "nr" (abyssd mobile). */
    static final Page.Choice NET_RAT = new Page.Choice("persist.abyss.mobile.rat",
            new String[] {"lte", "nr"}, new int[] {R.string.net_rat_lte, R.string.net_rat_nr},
            "lte");

    /** Switching window in seconds (abyssd uplink.rs: default 90, 0 = off, at most 600). */
    static final Page.Choice NET_WINDOW = Page.Choice.ints("persist.abyss.net.switch_window_s",
            new int[] {90, 30, 0},
            new int[] {R.string.net_window_90, R.string.net_window_30, R.string.net_window_off},
            90, 0, 600, R.string.screen_timeout_other);

    /** Experimental kernel suspend while idle (abyssd suspend.rs), off by default. */
    static final String PROP_SUSPEND = "persist.abyss.suspend";

    /**
     * Tuning that has no row (set from a shell; abyssd README "Properties"). Advanced shows the
     * ones that are set, read-only, so a forgotten hand-test value is visible.
     */
    private static final String[] TUNING = {
            "persist.abyss.audio.card", "persist.abyss.audio.device",
            "persist.abyss.audio.paths", "persist.abyss.audio.mixer_xml",
            "persist.abyss.audio.offload", "persist.abyss.audio.offload_device",
            "persist.abyss.audio.offload_paths", "persist.abyss.audio.offload_frags",
            "persist.abyss.audio.bt_paths", "persist.abyss.audio.bt_offload_paths",
            "persist.abyss.bt.ldac_abr_map", "persist.abyss.bt.aac_fmt_flag",
            "persist.abyss.bt.abr_pcm",
            "persist.abyss.wifi.linger_s", "persist.abyss.wifi.linger_max_s",
            "persist.abyss.wifi.mac", "persist.abyss.wifi.test_url",
            "persist.abyss.net.wifi_retry_s",
            "persist.abyss.mobile.linger_s",
            "persist.abyss.mobile.linger_max_s", "persist.abyss.mobile.idle_s",
            "persist.abyss.mobile.dormant", "persist.abyss.mobile.call_linger_max_s",
    };

    // ---- Sections --------------------------------------------------------------------------

    /** One entry of the top page's list. */
    static final class Section {
        final String id;
        final int title;
        final Function<Context, CharSequence> summary;
        final Consumer<Page> build;
        final Class<?> activity;

        Section(String id, int title, Function<Context, CharSequence> summary,
                Consumer<Page> build, Class<?> activity) {
            this.id = id;
            this.title = title;
            this.summary = summary;
            this.build = build;
            this.activity = activity;
        }
    }

    /** The sections, in the order the top page lists them. */
    static final Section[] ALL = {
            new Section("playback", R.string.sec_playback, Sections::playbackSummary,
                    Sections::playback, null),
            new Section("internet", R.string.sec_internet, Sections::internetSummary,
                    Sections::internet, null),
            new Section("navigation", R.string.sec_navigation, Sections::navigationSummary,
                    Sections::navigation, null),
            new Section("screen", R.string.sec_screen, Sections::screenSummary,
                    Sections::screen, null),
            new Section("modules", R.string.mod_title, Sections::modulesSummary, null,
                    ModulesActivity.class),
            new Section("captures", R.string.sec_captures_title, CapturesRows::summary,
                    p -> new CapturesRows(p).build(), null),
            new Section("before", R.string.sec_before, c -> c.getString(R.string.sec_before_sum),
                    Sections::before, null),
            new Section("advanced", R.string.sec_advanced, Sections::advancedSummary,
                    Sections::advanced, null),
    };

    static Section find(String id) {
        for (Section s : ALL) {
            if (s.id.equals(id)) {
                return s;
            }
        }
        return null;
    }

    /** The top page: the few settings people change between sessions. */
    static void quick(Page p) {
        Page.Group g = p.group(R.string.sec_quick);
        g.choice(R.string.net_mode_title, 0, NET_MODE);
        brightness(g);
    }

    /** The top page's list of sections. */
    static void list(Page p) {
        Page.Group g = p.group(R.string.sec_all);
        for (Section s : ALL) {
            g.nav(s.title, () -> s.summary.apply(p.a), () -> p.start(s.activity != null
                    ? new Intent(p.a, s.activity)
                    : SectionActivity.intent(p.a, s.id)));
        }
    }

    // Playback & Bluetooth ...................................................................

    static void playback(Page p) {
        p.intro(p.a.getString(R.string.next_entry_note));
        new BluetoothRows(p).build(p.group(R.string.bt_title));

        Page.Group q = p.group(R.string.sec_bt_quality);
        q.choice(R.string.bt_codec_title, R.string.bt_codec_summary, BT_CODEC);
        q.choice(R.string.bt_quality_title, R.string.bt_quality_summary, BT_LDAC_QUALITY);
        q.toggle(R.string.bt_offload_title, R.string.bt_offload_summary, PROP_BT_OFFLOAD, false,
                true);

        Page.Group v = p.group(R.string.sec_volume);
        v.info(R.string.volume_title, R.string.volume_summary);
    }

    private static CharSequence playbackSummary(Context c) {
        List<String> parts = new ArrayList<>();
        String last = BluetoothRows.lastName();
        if (last != null) {
            parts.add(last);
        }
        String codec = BT_CODEC.current(c).toString();
        if (BT_CODEC.resolve(SystemProperties.get(BT_CODEC.prop, "")) == 0) {
            codec += " " + BT_LDAC_QUALITY.current(c);
        }
        parts.add(codec);
        if (SystemProperties.getBoolean(PROP_BT_OFFLOAD, false)) {
            parts.add(c.getString(R.string.bt_offload_short));
        }
        return TextUtils.join(" · ", parts);
    }

    // Internet ...............................................................................

    static void internet(Page p) {
        p.intro(p.a.getString(R.string.next_entry_note));
        Page.Group g = p.group(0);
        g.choice(R.string.net_mode_title, R.string.net_mode_summary, NET_MODE);
        g.choice(R.string.net_format_title, R.string.net_format_summary, NET_FORMAT);
        g.choice(R.string.net_rat_title, R.string.net_rat_summary, NET_RAT);
        g.choice(R.string.net_window_title, R.string.net_window_summary, NET_WINDOW);
    }

    private static CharSequence internetSummary(Context c) {
        return NET_MODE.current(c) + " · " + c.getString(R.string.net_format_short,
                NET_FORMAT.current(c));
    }

    // Navigation & keys ......................................................................

    static void navigation(Page p) {
        p.intro(p.a.getString(R.string.next_entry_note));
        Page.Group g = p.group(0);
        g.choice(R.string.nav_start_title, R.string.nav_start_summary, NAV_START);
        Page.Group k = p.group(R.string.sec_keys);
        k.paragraph(0, p.a.getString(R.string.nav_keys_note));
        k.paragraph(0, p.a.getString(R.string.back_body));
    }

    private static CharSequence navigationSummary(Context c) {
        return c.getString(R.string.nav_start_short, NAV_START.current(c));
    }

    // Screen & power .........................................................................

    static void screen(Page p) {
        p.intro(p.a.getString(R.string.next_entry_note));
        Page.Group g = p.group(R.string.sec_screen_group);
        brightness(g);
        g.choice(R.string.screen_timeout_title, R.string.screen_timeout_summary, SCREEN_TIMEOUT);

        Page.Group pw = p.group(R.string.sec_power);
        pw.toggle(R.string.suspend_title, R.string.suspend_summary, PROP_SUSPEND, false, true);
        android.widget.TextView bat = pw.paragraph(R.string.battery_title, "");
        p.onRefresh(() -> bat.setText(Status.batteryDetail(p.a)));
        pw.paragraph(0, p.a.getString(R.string.lite_note));
    }

    private static CharSequence screenSummary(Context c) {
        return c.getString(R.string.screen_short, SCREEN_TIMEOUT.current(c),
                clampBrightness(SystemProperties.getInt(PROP_BRIGHTNESS, BRIGHTNESS_DEFAULT)),
                c.getString(SystemProperties.getBoolean(PROP_SUSPEND, false)
                        ? R.string.suspend_short_on : R.string.suspend_short_off));
    }

    /** Brightness in Abyss: persist.abyss.brightness 1..486 (top page and Screen & power). */
    static void brightness(Page.Group g) {
        Context c = g.card.getContext();
        g.slider(R.string.brightness_title, PROP_BRIGHTNESS, BRIGHTNESS_MIN, BRIGHTNESS_MAX,
                BRIGHTNESS_DEFAULT, level -> level == BRIGHTNESS_DEFAULT
                        ? c.getString(R.string.brightness_default, level)
                        : c.getString(R.string.brightness_value, level));
    }

    private static int clampBrightness(int v) {
        return Math.max(BRIGHTNESS_MIN, Math.min(BRIGHTNESS_MAX, v));
    }

    // Modules ................................................................................

    private static CharSequence modulesSummary(Context c) {
        try {
            List<String> names = new ArrayList<>();
            for (ModuleStore.Installed m : new ModuleStore(new File(ModuleStore.ROOT)).list()) {
                names.add(m.name());
            }
            return names.isEmpty() ? c.getString(R.string.mod_none) : TextUtils.join(", ", names);
        } catch (RuntimeException e) {
            return null;
        }
    }

    // Before you enter .......................................................................

    static void before(Page p) {
        Page.Group g = p.group(0);
        g.paragraph(0, p.a.getString(R.string.deep_body));
        g.paragraph(R.string.stops_heading, p.a.getString(R.string.stops_body));
        g.paragraph(R.string.keeps_heading, p.a.getString(R.string.keeps_body));
        g.paragraph(R.string.back_heading, p.a.getString(R.string.back_body));
        g.paragraph(0, p.a.getString(R.string.lite_note));
    }

    // Advanced ...............................................................................

    static void advanced(Page p) {
        Page.Group g = p.group(R.string.sec_diag);
        android.widget.TextView state = g.paragraph(R.string.adv_state_title, "");
        p.onRefresh(() -> {
            String s = SystemProperties.get(EntryActivity.PROP_STATE, "");
            state.setText(s.isEmpty() ? p.a.getString(R.string.not_installed) : s);
        });
        android.widget.TextView tuning = g.paragraph(R.string.adv_tuning_title, "");
        p.onRefresh(() -> tuning.setText(tuningText(p.a)));

        Page.Group next = p.group(R.string.sec_coming_next);
        ComingNext.add(p, next, R.string.sec_scripts_title, "v1.1", R.string.scripts_body,
                R.string.docs_scripts, R.string.docs_path_scripts);
        ComingNext.add(p, next, R.string.sec_apps_title, "v1.2", R.string.apps_body,
                R.string.docs_apps, R.string.docs_path_apps);
    }

    private static CharSequence advancedSummary(Context c) {
        int n = 0;
        for (String prop : TUNING) {
            if (!SystemProperties.get(prop, "").isEmpty()) {
                n++;
            }
        }
        return n == 0 ? c.getString(R.string.sec_advanced_sum)
                : c.getString(R.string.adv_tuning_count, n);
    }

    private static CharSequence tuningText(Context c) {
        StringBuilder b = new StringBuilder();
        for (String prop : TUNING) {
            String v = SystemProperties.get(prop, "");
            if (!v.isEmpty()) {
                if (b.length() > 0) {
                    b.append('\n');
                }
                b.append(prop.substring("persist.abyss.".length())).append(" = ").append(v);
            }
        }
        return b.length() == 0 ? c.getString(R.string.adv_tuning_none)
                : c.getString(R.string.adv_tuning_hint) + "\n\n" + b;
    }
}
