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

import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Context;
import android.content.Intent;
import android.content.res.ColorStateList;
import android.graphics.Typeface;
import android.graphics.drawable.Drawable;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.graphics.drawable.StateListDrawable;
import android.os.SystemProperties;
import android.provider.Settings;
import android.text.TextUtils;
import android.util.Log;
import android.util.SparseArray;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.ImageButton;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.SeekBar;
import android.widget.Switch;
import android.widget.TextView;

import java.util.ArrayList;
import java.util.List;
import java.util.function.IntFunction;
import java.util.function.Supplier;

/**
 * The control center's row builder. Every page of Settings › Abyss (the top page,
 * {@link EntryActivity}, and each section, {@link SectionActivity}) is a Page: a header, then
 * {@link Group}s (an optional small upper-case title over one card), each holding rows. Which rows
 * go on which page is decided in one place, {@link Sections}; this class only draws them.
 *
 * <p>Look: the IchthysOS "Shoal" Settings look, from the framework's ichthys_* theme tokens
 * (res/values/colors.xml maps the abyss_* names onto them, so light/dark and the theme's accent
 * follow the device). Cards are the theme's card fill, outlined by a hairline unless
 * theme.surface is "tonal"; the accent marks only the selected choice, switches, sliders and the
 * one primary button. Dark mode stays OLED-friendly: the page ground is ichthys_ink (black with
 * theme.pitchBlack) and nothing large is filled.
 *
 * <p>Rows ({@link Group}): {@code nav} (drill-in, live summary; {@code link} for rows rebuilt
 * on refresh), {@code choice} (one tap = one
 * value written to a property, {@link Choice}), {@code toggle} (0/1 property), {@code slider},
 * {@code info} (title + text), {@code paragraph}, {@code view} (anything else). Each row
 * that shows a value registers a refresher; {@link #refresh()} (called from onResume) re-reads
 * every property, so a value changed from a shell or from another page shows up.
 */
final class Page {
    private static final String TAG = "AbyssEntry";
    /** Settings.Secure theme.surface ("tonal" = filled cards without a stroke). */
    private static final String KEY_SURFACE = "ichthys_theme_surface";
    private static final String KEY_FONT_DISPLAY = "ichthys_theme_font_display";
    private static final String KEY_FONT_TEXT = "ichthys_theme_font_text";
    private static final int FIRST_REQUEST = 100;

    /** A result from {@link #startForResult}. */
    interface Result {
        void onResult(int resultCode, Intent data);
    }

    final Activity a;
    private final LinearLayout mCol;
    private final TextView mError;
    private final List<Runnable> mRefreshers = new ArrayList<>();
    private final SparseArray<Result> mResults = new SparseArray<>();
    private int mNextRequest = FIRST_REQUEST;
    private TextView mLastTitle;
    private final Typeface mDisplay;
    private final Typeface mText;
    private final Typeface mTextMedium;

    /**
     * Sets {@code a}'s content to a new scrolling page with {@code title}; {@code back} adds the
     * Up arrow (finishes the activity).
     */
    Page(Activity a, CharSequence title, boolean back) {
        this.a = a;
        mDisplay = face(a, KEY_FONT_DISPLAY, com.android.internal.R.string.ichthys_font_display,
                600);
        mText = face(a, KEY_FONT_TEXT, com.android.internal.R.string.ichthys_font_text, 400);
        mTextMedium = face(a, KEY_FONT_TEXT, com.android.internal.R.string.ichthys_font_text, 500);

        ScrollView sv = new ScrollView(a);
        sv.setBackgroundColor(a.getColor(R.color.abyss_bg));
        sv.setFitsSystemWindows(true);
        sv.setFillViewport(true);
        mCol = new LinearLayout(a);
        mCol.setOrientation(LinearLayout.VERTICAL);
        mCol.setPadding(dp(16), dp(back ? 4 : 28), dp(16), dp(40));
        sv.addView(mCol, new ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT));
        a.setContentView(sv);

        if (back) {
            ImageButton up = new ImageButton(a);
            TypedValue tv = new TypedValue();
            a.getTheme().resolveAttribute(android.R.attr.homeAsUpIndicator, tv, true);
            up.setImageResource(tv.resourceId);
            up.setImageTintList(ColorStateList.valueOf(a.getColor(R.color.abyss_text)));
            a.getTheme().resolveAttribute(android.R.attr.selectableItemBackgroundBorderless, tv,
                    true);
            up.setBackgroundResource(tv.resourceId);
            up.setContentDescription(a.getString(R.string.navigate_up));
            up.setOnClickListener(v -> a.finish());
            LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(dp(48), dp(48));
            lp.setMarginStart(-dp(4));
            mCol.addView(up, lp);
        }
        TextView t = new TextView(a);
        t.setText(title);
        t.setTypeface(mDisplay);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, back ? 32 : 40);
        t.setTextColor(a.getColor(R.color.abyss_text));
        t.setPadding(dp(4), dp(back ? 12 : 0), dp(4), 0);
        t.setAccessibilityHeading(true);
        mCol.addView(t, lp(0));
        a.setTitle(title);

        mError = new TextView(a);
        mError.setTextColor(a.getColor(R.color.abyss_warn));
        mError.setTextSize(TypedValue.COMPLEX_UNIT_SP, 15);
        mError.setPadding(dp(4), 0, dp(4), 0);
        mError.setVisibility(View.GONE);
        mCol.addView(mError, lp(12));
    }

    /** A dim line under the title. */
    TextView intro(CharSequence s) {
        TextView t = text(s, 15, R.color.abyss_dim, mText);
        t.setPadding(dp(4), 0, dp(4), 0);
        mCol.addView(t, lp(8));
        return t;
    }

    /** A group: {@code title} (0 = none) over one card. */
    Group group(int title) {
        return group(title == 0 ? null : a.getString(title));
    }

    /** The title view of the last {@link #group} (null when it had none). */
    TextView lastGroupTitle() {
        return mLastTitle;
    }

    Group group(CharSequence title) {
        mLastTitle = null;
        if (title != null) {
            TextView h = text(title.toString().toUpperCase(), 12, R.color.abyss_dim, face(a,
                    KEY_FONT_TEXT, com.android.internal.R.string.ichthys_font_text, 600));
            h.setLetterSpacing(0.08f);
            h.setPadding(dp(16), 0, dp(16), 0);
            h.setAccessibilityHeading(true);
            LinearLayout.LayoutParams l = lp(28);
            l.bottomMargin = dp(8);
            mCol.addView(h, l);
            mLastTitle = h;
        }
        LinearLayout card = new LinearLayout(a);
        card.setOrientation(LinearLayout.VERTICAL);
        card.setBackground(cardDrawable());
        card.setClipToOutline(true);
        mCol.addView(card, lp(title == null ? 20 : 0));
        return new Group(card);
    }

    /** Re-reads every value shown on the page. */
    void refresh() {
        for (Runnable r : mRefreshers) {
            r.run();
        }
    }

    /** Registers {@code r} for {@link #refresh()} and runs it once now. */
    void onRefresh(Runnable r) {
        mRefreshers.add(r);
        r.run();
    }

    /** persist.abyss.* = abyss_config_prop, settable by system_app (abyss_entry.te). */
    boolean setProp(String name, String value) {
        try {
            SystemProperties.set(name, value);
            mError.setVisibility(View.GONE);
            return true;
        } catch (RuntimeException e) {
            Log.e(TAG, "setprop " + name + " failed", e);
            mError.setText(R.string.setting_failed);
            mError.setVisibility(View.VISIBLE);
            return false;
        }
    }

    void startForResult(Intent i, Result r) {
        int code = mNextRequest++;
        mResults.put(code, r);
        a.startActivityForResult(i, code);
    }

    /** Forwarded from the activity's onActivityResult. */
    void onActivityResult(int requestCode, int resultCode, Intent data) {
        Result r = mResults.get(requestCode);
        if (r != null) {
            mResults.remove(requestCode);
            r.onResult(resultCode, data);
        }
    }

    void start(Intent i) {
        try {
            a.startActivity(i);
        } catch (ActivityNotFoundException e) {
            Log.w(TAG, "no activity for " + i, e);
        }
    }

    /** Adds {@code v} straight to the page column (outside any card). */
    void addToPage(View v, int topDp) {
        mCol.addView(v, lp(topDp));
    }

    // ---- Groups and rows -------------------------------------------------------------------

    /** One card; rows are separated by an inset hairline. */
    final class Group {
        final LinearLayout card;

        Group(LinearLayout card) {
            this.card = card;
        }

        private LinearLayout add(LinearLayout row) {
            if (card.getChildCount() > 0) {
                View rule = new View(a);
                rule.setBackgroundColor(a.getColor(R.color.abyss_rule));
                LinearLayout.LayoutParams l = new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT, Math.max(1, dp(1)));
                l.setMarginStart(dp(16));
                l.setMarginEnd(dp(16));
                card.addView(rule, l);
            }
            card.addView(row, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT));
            return row;
        }

        private LinearLayout column() {
            LinearLayout r = new LinearLayout(a);
            r.setOrientation(LinearLayout.VERTICAL);
            r.setPadding(dp(16), dp(14), dp(16), dp(14));
            return r;
        }

        private void clickable(View r, View.OnClickListener l) {
            TypedValue tv = new TypedValue();
            a.getTheme().resolveAttribute(android.R.attr.selectableItemBackground, tv, true);
            r.setBackgroundResource(tv.resourceId);
            r.setClickable(true);
            r.setFocusable(true);
            r.setOnClickListener(l);
        }

        /**
         * A drill-in row: title, a live summary ({@code summary} may be null; re-read on
         * refresh) and a chevron. {@code click} runs on tap.
         */
        LinearLayout nav(CharSequence title, Supplier<CharSequence> summary, Runnable click) {
            return navRow(title, summary, null, click);
        }

        /**
         * A drill-in row with a fixed summary: for rows that are rebuilt on refresh ({@link
         * #clear} first), like the list of installed apps.
         */
        LinearLayout link(CharSequence title, CharSequence summary, Runnable click) {
            return navRow(title, null, summary, click);
        }

        /** Removes every row (for a group whose rows are rebuilt with {@link #link}). */
        void clear() {
            card.removeAllViews();
        }

        private LinearLayout navRow(CharSequence title, Supplier<CharSequence> summary,
                CharSequence fixed, Runnable click) {
            LinearLayout r = new LinearLayout(a);
            r.setOrientation(LinearLayout.HORIZONTAL);
            r.setGravity(Gravity.CENTER_VERTICAL);
            r.setPadding(dp(16), dp(14), dp(12), dp(14));
            r.setMinimumHeight(dp(64));
            LinearLayout texts = new LinearLayout(a);
            texts.setOrientation(LinearLayout.VERTICAL);
            texts.addView(title(title));
            TextView sum = summary(null);
            texts.addView(sum, lp(2));
            r.addView(texts, new LinearLayout.LayoutParams(0,
                    ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
            TextView chev = text("›", 26, R.color.abyss_dim, mText);
            chev.setImportantForAccessibility(View.IMPORTANT_FOR_ACCESSIBILITY_NO);
            chev.setPadding(dp(12), 0, dp(4), dp(3));
            r.addView(chev);
            clickable(r, v -> click.run());
            if (summary != null) {
                onRefresh(() -> setOrHide(sum, summary.get()));
            } else {
                setOrHide(sum, fixed);
            }
            return add(r);
        }

        LinearLayout nav(int title, Supplier<CharSequence> summary, Runnable click) {
            return nav(a.getString(title), summary, click);
        }

        /**
         * One tap per value: a row of segments, the current value marked; a tap writes the
         * segment's value to {@link Choice#prop}. Unset or unknown values show the default.
         */
        LinearLayout choice(int title, int summary, Choice c) {
            LinearLayout r = column();
            r.addView(title(a.getString(title)));
            if (summary != 0) {
                r.addView(summary(a.getString(summary)), lp(2));
            }
            LinearLayout seg = new LinearLayout(a);
            seg.setOrientation(LinearLayout.HORIZONTAL);
            TextView other = summary(null);
            other.setVisibility(View.GONE);
            Runnable show = () -> {
                String raw = SystemProperties.get(c.prop, "");
                int sel = c.resolve(raw);
                for (int i = 0; i < seg.getChildCount(); i++) {
                    TextView chip = (TextView) seg.getChildAt(i);
                    boolean on = i == sel;
                    chip.setSelected(on);
                    chip.setTypeface(on ? mTextMedium : mText);
                }
                // A value set from a shell (e.g. 20 s) is shown as it is, not silently replaced.
                CharSequence o = sel < 0 ? c.otherText(a, raw) : null;
                setOrHide(other, o);
            };
            for (int i = 0; i < c.size(); i++) {
                String value = c.values[i];
                TextView chip = text(c.label(a, i), 14, R.color.abyss_text, mText);
                chip.setGravity(Gravity.CENTER);
                chip.setMaxLines(2);
                chip.setMinHeight(dp(44));
                chip.setPadding(dp(8), dp(6), dp(8), dp(6));
                chip.setBackground(chipDrawable());
                chip.setClickable(true);
                chip.setFocusable(true);
                chip.setOnClickListener(v -> {
                    if (setProp(c.prop, value)) {
                        show.run();
                    }
                });
                LinearLayout.LayoutParams l = new LinearLayout.LayoutParams(0,
                        ViewGroup.LayoutParams.WRAP_CONTENT, 1f);
                if (i > 0) {
                    l.setMarginStart(dp(8));
                }
                seg.addView(chip, l);
            }
            r.addView(seg, lp(12));
            r.addView(other, lp(8));
            onRefresh(show);
            return add(r);
        }

        /**
         * A switch writing "1" / "0" to {@code prop} (unset = {@code def}); a refused write puts
         * the switch back. {@code warn}: the summary is a caution (experimental features).
         */
        LinearLayout toggle(int title, int summary, String prop, boolean def, boolean warn) {
            LinearLayout r = new LinearLayout(a);
            r.setOrientation(LinearLayout.HORIZONTAL);
            r.setGravity(Gravity.CENTER_VERTICAL);
            r.setPadding(dp(16), dp(14), dp(16), dp(14));
            LinearLayout texts = new LinearLayout(a);
            texts.setOrientation(LinearLayout.VERTICAL);
            texts.addView(title(a.getString(title)));
            if (summary != 0) {
                TextView s = summary(a.getString(summary));
                if (warn) {
                    s.setTextColor(a.getColor(R.color.abyss_warn));
                }
                texts.addView(s, lp(2));
            }
            r.addView(texts, new LinearLayout.LayoutParams(0,
                    ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
            Switch sw = new Switch(a);
            sw.setPadding(dp(16), 0, 0, 0);
            sw.setContentDescription(a.getString(title));
            r.addView(sw);
            boolean[] binding = {false};
            sw.setOnCheckedChangeListener((b, on) -> {
                if (binding[0]) {
                    return;
                }
                if (!setProp(prop, on ? "1" : "0")) {
                    binding[0] = true;
                    b.setChecked(!on);
                    binding[0] = false;
                }
            });
            clickable(r, v -> sw.toggle());
            onRefresh(() -> {
                binding[0] = true;
                sw.setChecked(SystemProperties.getBoolean(prop, def));
                binding[0] = false;
            });
            return add(r);
        }

        /**
         * A slider for an integer property ({@code min..max}, unset = {@code def}); the value
         * line comes from {@code label}. Saved when the finger lifts, and on every keyboard /
         * accessibility step (those have no start/stop).
         */
        LinearLayout slider(int title, String prop, int min, int max, int def,
                IntFunction<CharSequence> label) {
            LinearLayout r = column();
            r.addView(title(a.getString(title)));
            TextView value = summary(null);
            r.addView(value, lp(2));
            SeekBar bar = new SeekBar(a);
            bar.setMin(min);
            bar.setMax(max);
            bar.setContentDescription(a.getString(title));
            bar.setProgressTintList(ColorStateList.valueOf(a.getColor(R.color.abyss_accent)));
            bar.setThumbTintList(ColorStateList.valueOf(a.getColor(R.color.abyss_accent)));
            bar.setProgressBackgroundTintList(
                    ColorStateList.valueOf(a.getColor(R.color.abyss_track)));
            boolean[] tracking = {false};
            bar.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
                @Override
                public void onProgressChanged(SeekBar s, int progress, boolean fromUser) {
                    value.setText(label.apply(progress));
                    if (fromUser && !tracking[0]) {
                        setProp(prop, Integer.toString(progress));
                    }
                }

                @Override
                public void onStartTrackingTouch(SeekBar s) {
                    tracking[0] = true;
                }

                @Override
                public void onStopTrackingTouch(SeekBar s) {
                    tracking[0] = false;
                    setProp(prop, Integer.toString(s.getProgress()));
                }
            });
            LinearLayout.LayoutParams l = lp(8);
            l.setMarginStart(-dp(12));
            l.setMarginEnd(-dp(12));
            r.addView(bar, l);
            onRefresh(() -> {
                int v = Math.max(min, Math.min(max, SystemProperties.getInt(prop, def)));
                bar.setProgress(v);
                value.setText(label.apply(v));
            });
            return add(r);
        }

        /** A title over a paragraph (information, nothing to set). */
        LinearLayout info(int title, CharSequence body) {
            LinearLayout r = column();
            r.addView(title(a.getString(title)));
            r.addView(summary(body), lp(2));
            return add(r);
        }

        LinearLayout info(int title, int body) {
            return info(title, a.getString(body));
        }

        /** A paragraph in the card; {@code heading} (0 = none) above it in the accent. */
        TextView paragraph(int heading, CharSequence body) {
            LinearLayout r = column();
            if (heading != 0) {
                TextView h = text(a.getString(heading), 15, R.color.abyss_accent,
                        mTextMedium);
                r.addView(h);
            }
            TextView b = text(body, 15, R.color.abyss_text, mText);
            b.setLineSpacing(0, 1.15f);
            r.addView(b, lp(heading == 0 ? 0 : 4));
            add(r);
            return b;
        }

        /** Anything else (a list, buttons): padded like a row. */
        LinearLayout view(View v) {
            LinearLayout r = column();
            r.addView(v, lp(0));
            return add(r);
        }
    }

    // ---- Choice ----------------------------------------------------------------------------

    /**
     * A property with a fixed set of values (one segment each). Unset or unknown = {@link #def}.
     * Numeric choices ({@link #numeric}) also accept any value in {@code min..max} set from a
     * shell and show it as {@code otherFmt} instead of a segment.
     */
    static final class Choice {
        final String prop;
        final String[] values;
        private final int[] labels;
        private final int labelFmt;
        final String def;
        private final boolean numeric;
        private final int min;
        private final int max;
        private final int otherFmt;

        /** Labels from string resources, one per value. */
        Choice(String prop, String[] values, int[] labels, String def) {
            this(prop, values, labels, 0, def, false, 0, 0, 0);
        }

        /** Integer values, labelled with {@code labelFmt} (e.g. "%1$d s"). */
        static Choice ints(String prop, int[] values, int labelFmt, int def, int min, int max,
                int otherFmt) {
            String[] v = new String[values.length];
            for (int i = 0; i < values.length; i++) {
                v[i] = Integer.toString(values[i]);
            }
            return new Choice(prop, v, null, labelFmt, Integer.toString(def), true, min, max,
                    otherFmt);
        }

        /** Integer values with their own label strings (e.g. "Off" for 0). */
        static Choice ints(String prop, int[] values, int[] labels, int def, int min, int max,
                int otherFmt) {
            String[] v = new String[values.length];
            for (int i = 0; i < values.length; i++) {
                v[i] = Integer.toString(values[i]);
            }
            return new Choice(prop, v, labels, 0, Integer.toString(def), true, min, max,
                    otherFmt);
        }

        private Choice(String prop, String[] values, int[] labels, int labelFmt, String def,
                boolean numeric, int min, int max, int otherFmt) {
            this.prop = prop;
            this.values = values;
            this.labels = labels;
            this.labelFmt = labelFmt;
            this.def = def;
            this.numeric = numeric;
            this.min = min;
            this.max = max;
            this.otherFmt = otherFmt;
        }

        int size() {
            return values.length;
        }

        CharSequence label(Context c, int i) {
            return labels != null ? c.getString(labels[i])
                    : c.getString(labelFmt, Integer.parseInt(values[i]));
        }

        /** The segment for {@code raw}; -1 = a numeric value outside the segments. */
        int resolve(String raw) {
            String cur = raw == null ? "" : raw.trim();
            if (numeric) {
                int n;
                try {
                    n = Integer.parseInt(cur);
                } catch (NumberFormatException e) {
                    n = Integer.parseInt(def);
                }
                if (n < min || n > max) {
                    n = Integer.parseInt(def);
                }
                cur = Integer.toString(n);
            }
            int found = -1;
            for (int i = 0; i < values.length; i++) {
                if (values[i].equalsIgnoreCase(cur)) {
                    found = i;
                }
            }
            if (found >= 0 || numeric) {
                return found;
            }
            for (int i = 0; i < values.length; i++) {
                if (values[i].equals(def)) {
                    return i;
                }
            }
            return 0;
        }

        /** The current value's label (for summaries). */
        CharSequence current(Context c) {
            String raw = SystemProperties.get(prop, "");
            int i = resolve(raw);
            return i >= 0 ? label(c, i) : otherText(c, raw);
        }

        CharSequence otherText(Context c, String raw) {
            if (!numeric || otherFmt == 0) {
                return null;
            }
            try {
                return c.getString(otherFmt, Integer.parseInt(raw.trim()));
            } catch (NumberFormatException e) {
                return null;
            }
        }
    }

    // ---- Look ------------------------------------------------------------------------------

    int dp(float v) {
        return Math.round(v * a.getResources().getDisplayMetrics().density);
    }

    LinearLayout.LayoutParams lp(int topDp) {
        LinearLayout.LayoutParams p = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        p.topMargin = dp(topDp);
        return p;
    }

    TextView text(CharSequence s, float sp, int colorRes, Typeface face) {
        TextView t = new TextView(a);
        t.setText(s);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        if (colorRes != 0) {
            t.setTextColor(a.getColor(colorRes));
        }
        t.setTypeface(face);
        return t;
    }

    /** A row title (body font, medium). */
    TextView title(CharSequence s) {
        return text(s, 16.5f, R.color.abyss_text, mTextMedium);
    }

    /** A row summary (body font, muted). */
    TextView summary(CharSequence s) {
        TextView t = text(s, 14, R.color.abyss_dim, mText);
        t.setLineSpacing(0, 1.1f);
        return t;
    }

    Typeface bodyFace(boolean medium) {
        return medium ? mTextMedium : mText;
    }

    static void setOrHide(TextView t, CharSequence s) {
        t.setText(s);
        t.setVisibility(TextUtils.isEmpty(s) ? View.GONE : View.VISIBLE);
    }

    private boolean tonal() {
        return "tonal".equals(Settings.Secure.getString(a.getContentResolver(), KEY_SURFACE));
    }

    private float radius() {
        return a.getResources().getDimension(com.android.internal.R.dimen.ichthys_radius);
    }

    /** The group card: theme card fill, hairline outline unless theme.surface is tonal. */
    Drawable cardDrawable() {
        GradientDrawable g = new GradientDrawable();
        g.setColor(a.getColor(R.color.abyss_card));
        g.setCornerRadius(radius());
        if (!tonal()) {
            g.setStroke(Math.max(1, dp(1)), a.getColor(R.color.abyss_rule));
        }
        return g;
    }

    /** Segment: outlined pill; selected = the accent's tint, no outline. */
    private Drawable chipDrawable() {
        float r = dp(22);
        GradientDrawable on = new GradientDrawable();
        on.setCornerRadius(r);
        on.setColor(a.getColor(R.color.abyss_accent_tint));
        on.setStroke(Math.max(1, dp(1)), a.getColor(R.color.abyss_accent));
        GradientDrawable off = new GradientDrawable();
        off.setCornerRadius(r);
        off.setColor(0);
        off.setStroke(Math.max(1, dp(1)), a.getColor(R.color.abyss_outline));
        StateListDrawable s = new StateListDrawable();
        s.addState(new int[] {android.R.attr.state_selected}, on);
        s.addState(new int[0], off);
        GradientDrawable mask = new GradientDrawable();
        mask.setCornerRadius(r);
        mask.setColor(0xFFFFFFFF);
        return new RippleDrawable(ColorStateList.valueOf(ripple()), s, mask);
    }

    /** The one primary action of a page (Enter Abyss): filled accent pill. */
    Drawable primaryDrawable() {
        float r = dp(28);
        GradientDrawable on = new GradientDrawable();
        on.setCornerRadius(r);
        on.setColor(a.getColor(R.color.abyss_accent));
        GradientDrawable off = new GradientDrawable();
        off.setCornerRadius(r);
        off.setColor(a.getColor(R.color.abyss_track));
        StateListDrawable s = new StateListDrawable();
        s.addState(new int[] {-android.R.attr.state_enabled}, off);
        s.addState(new int[0], on);
        return new RippleDrawable(ColorStateList.valueOf(ripple()), s, null);
    }

    /** A secondary action: outlined pill. */
    Drawable outlineDrawable() {
        float r = dp(22);
        GradientDrawable g = new GradientDrawable();
        g.setCornerRadius(r);
        g.setColor(0);
        g.setStroke(Math.max(1, dp(1)), a.getColor(R.color.abyss_outline));
        GradientDrawable mask = new GradientDrawable();
        mask.setCornerRadius(r);
        mask.setColor(0xFFFFFFFF);
        return new RippleDrawable(ColorStateList.valueOf(ripple()), g, mask);
    }

    private int ripple() {
        return (a.getColor(R.color.abyss_text) & 0x00FFFFFF) | 0x1F000000;
    }

    /**
     * A theme font family: Settings.Secure {@code secureKey} (theme.font.*), else the framework
     * token string (written by IchthysConfigService), at {@code weight}.
     */
    private static Typeface face(Context c, String secureKey, int tokenRes, int weight) {
        String name = Settings.Secure.getString(c.getContentResolver(), secureKey);
        if (TextUtils.isEmpty(name)) {
            try {
                name = c.getString(tokenRes);
            } catch (RuntimeException e) {
                name = null;
            }
        }
        Typeface family = TextUtils.isEmpty(name) ? Typeface.DEFAULT
                : Typeface.create(name, Typeface.NORMAL);
        return Typeface.create(family, weight, false);
    }
}
