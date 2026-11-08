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
import android.content.Context;
import android.graphics.Typeface;
import android.text.Editable;
import android.text.InputType;
import android.text.TextWatcher;
import android.util.TypedValue;
import android.view.HapticFeedbackConstants;
import android.view.InputDevice;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowManager;
import android.widget.Button;
import android.widget.CompoundButton;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.RadioButton;
import android.widget.RadioGroup;
import android.widget.ScrollView;
import android.widget.Switch;
import android.widget.TextView;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Views for the Modules pages, built in code with the control center's styles (pure black,
 * outlines not fills). Kept out of center.xml so the Modules pages add no shared layout.
 */
final class ModuleUi {
    private ModuleUi() {
    }

    static int dp(Context c, float v) {
        return (int) TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v,
                c.getResources().getDisplayMetrics());
    }

    /** A full-screen scrolling page; returns the column to add to. FLAG_SECURE when asked. */
    static LinearLayout page(Activity a, boolean secure) {
        if (secure) {
            a.getWindow().addFlags(WindowManager.LayoutParams.FLAG_SECURE);
        }
        ScrollView sv = new ScrollView(a);
        sv.setBackgroundColor(a.getColor(R.color.abyss_bg));
        sv.setFitsSystemWindows(true);
        sv.setFillViewport(true);
        LinearLayout col = new LinearLayout(a);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(dp(a, 20), dp(a, 28), dp(a, 20), dp(a, 40));
        sv.addView(col, new ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT));
        a.setContentView(sv);
        return col;
    }

    static LinearLayout.LayoutParams lp(Context c, int topDp) {
        LinearLayout.LayoutParams p = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        p.topMargin = dp(c, topDp);
        return p;
    }

    static TextView text(ViewGroup parent, int style, CharSequence s, int topDp) {
        Context c = parent.getContext();
        TextView t = new TextView(c, null, 0, style);
        t.setText(s);
        parent.addView(t, lp(c, topDp));
        return t;
    }

    static TextView title(ViewGroup p, CharSequence s) {
        TextView t = text(p, R.style.AbyssLabel, s, 0);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, 32);
        return t;
    }

    static TextView section(ViewGroup p, CharSequence s) {
        TextView t = text(p, R.style.AbyssSection, s.toString().toUpperCase(), 28);
        LinearLayout.LayoutParams l = (LinearLayout.LayoutParams) t.getLayoutParams();
        l.bottomMargin = dp(p.getContext(), 10);
        return t;
    }

    static TextView body(ViewGroup p, CharSequence s) {
        return text(p, R.style.AbyssBody, s, 6);
    }

    static TextView dim(ViewGroup p, CharSequence s) {
        return text(p, R.style.AbyssDim, s, 6);
    }

    static TextView warn(ViewGroup p, CharSequence s) {
        TextView t = dim(p, s);
        t.setTextColor(p.getContext().getColor(R.color.abyss_warn));
        return t;
    }

    static LinearLayout card(ViewGroup p) {
        Context c = p.getContext();
        LinearLayout l = new LinearLayout(c, null, 0, R.style.AbyssCard);
        l.setOrientation(LinearLayout.VERTICAL);
        l.setBackgroundResource(R.drawable.outline_card);
        int pad = dp(c, 16);
        l.setPadding(pad, pad, pad, pad);
        p.addView(l, lp(c, 0));
        return l;
    }

    static Button button(ViewGroup p, CharSequence s, View.OnClickListener l, int topDp) {
        Context c = p.getContext();
        Button b = new Button(c, null, 0, R.style.AbyssButton);
        b.setText(s);
        b.setOnClickListener(l);
        p.addView(b, lp(c, topDp));
        return b;
    }

    /** A tappable two-line row (title, dim line) with the list-row outline. */
    static LinearLayout row(ViewGroup p, CharSequence title, CharSequence sub,
            View.OnClickListener l) {
        Context c = p.getContext();
        LinearLayout r = new LinearLayout(c);
        r.setOrientation(LinearLayout.VERTICAL);
        r.setBackgroundResource(R.drawable.row_bg);
        r.setPadding(dp(c, 4), dp(c, 12), dp(c, 4), dp(c, 12));
        r.setClickable(true);
        r.setFocusable(true);
        r.setOnClickListener(l);
        text(r, R.style.AbyssLabel, title, 0);
        if (sub != null) {
            dim(r, sub).setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
        }
        p.addView(r, lp(c, 0));
        return r;
    }

    /**
     * The approval control: a person must HOLD it for one second with a finger on the real
     * touchscreen. Taps sent by accessibility services (performClick), injected or virtual
     * input, and touches while another app draws over the window are refused. This is the
     * "physical action on the phone" the approval rule asks for; best effort, not a proof.
     */
    static final class HoldToApprove extends TextView {
        interface Done {
            void approved();
        }

        private static final long HOLD_MS = 1000;
        private final CharSequence mLabel;
        private final Done mDone;
        private boolean mHolding;
        private final Runnable mFire = new Runnable() {
            @Override
            public void run() {
                if (!mHolding) {
                    return;
                }
                mHolding = false;
                setPressed(false);
                setText(mLabel);
                performHapticFeedback(HapticFeedbackConstants.CONFIRM);
                mDone.approved();
            }
        };

        HoldToApprove(Context c, CharSequence label, Done done) {
            super(c, null, 0, R.style.AbyssButton);
            mLabel = label;
            mDone = done;
            setText(label);
            setGravity(android.view.Gravity.CENTER);
            setBackgroundResource(R.drawable.outline_button);
            setTypeface(Typeface.DEFAULT_BOLD);
            setFilterTouchesWhenObscured(true);
            setClickable(true);
            setContentDescription(label + ". " + c.getString(R.string.mod_hold_hint));
        }

        static boolean realTouch(MotionEvent ev) {
            int tool = ev.getToolType(0);
            InputDevice d = ev.getDevice();
            return (tool == MotionEvent.TOOL_TYPE_FINGER || tool == MotionEvent.TOOL_TYPE_STYLUS)
                    && ev.isFromSource(InputDevice.SOURCE_TOUCHSCREEN)
                    && d != null && !d.isVirtual();
        }

        @Override
        public boolean onTouchEvent(MotionEvent ev) {
            if (!isEnabled()) {
                return false;
            }
            switch (ev.getActionMasked()) {
                case MotionEvent.ACTION_DOWN:
                    if ((ev.getFlags() & (MotionEvent.FLAG_WINDOW_IS_OBSCURED
                            | MotionEvent.FLAG_WINDOW_IS_PARTIALLY_OBSCURED)) != 0) {
                        setText(R.string.mod_hold_obscured);
                        return true;
                    }
                    if (!realTouch(ev)) {
                        setText(R.string.mod_hold_finger);
                        return true;
                    }
                    mHolding = true;
                    setPressed(true);
                    setText(R.string.mod_hold_keep);
                    postDelayed(mFire, HOLD_MS);
                    return true;
                case MotionEvent.ACTION_MOVE:
                    if (mHolding && (ev.getX() < 0 || ev.getY() < 0 || ev.getX() > getWidth()
                            || ev.getY() > getHeight() || ev.getPointerCount() > 1)) {
                        cancel();
                    }
                    return true;
                case MotionEvent.ACTION_UP:
                case MotionEvent.ACTION_CANCEL:
                    cancel();
                    return true;
                default:
                    return true;
            }
        }

        private void cancel() {
            if (mHolding) {
                mHolding = false;
                removeCallbacks(mFire);
                setPressed(false);
                setText(mLabel);
            }
        }

        /** No click path: accessibility "click" can't approve. */
        @Override
        public boolean performClick() {
            return false;
        }
    }

    static HoldToApprove hold(ViewGroup p, CharSequence label, HoldToApprove.Done d) {
        Context c = p.getContext();
        HoldToApprove h = new HoldToApprove(c, label, d);
        LinearLayout.LayoutParams l = lp(c, 18);
        h.setMinHeight(dp(c, 64));
        p.addView(h, l);
        return h;
    }

    /** Typed fields for a module's settings schema (text number bool enum secret). */
    static final class SettingsForm {
        private final Context mC;
        private final List<Field> mFields = new ArrayList<>();
        private Runnable mOnChange;

        private static final class Field {
            ModuleSpec.Setting s;
            EditText edit;
            CompoundButton toggle;
            RadioGroup group;
            List<String> groupValues = new ArrayList<>();
            TextView error;
        }

        SettingsForm(ViewGroup parent, ModuleSpec spec, Map<String, Object> values) {
            mC = parent.getContext();
            if (spec.settings.isEmpty()) {
                dim(parent, mC.getString(R.string.mod_no_settings));
            }
            boolean anySecret = false;
            for (ModuleSpec.Setting s : spec.settings) {
                Field f = new Field();
                f.s = s;
                Object v = values.containsKey(s.key) ? values.get(s.key) : s.def;
                text(parent, R.style.AbyssLabel, s.label
                        + (s.unit != null ? " (" + s.unit + ")" : ""), 18);
                if (s.help != null) {
                    dim(parent, s.help);
                }
                switch (s.type) {
                    case "bool": {
                        Switch sw = new Switch(mC);
                        sw.setText(R.string.mod_on);
                        sw.setTextColor(mC.getColor(R.color.abyss_text));
                        sw.setChecked(Boolean.TRUE.equals(v));
                        sw.setOnCheckedChangeListener((b, c) -> changed());
                        parent.addView(sw, lp(mC, 4));
                        f.toggle = sw;
                        break;
                    }
                    case "enum": {
                        RadioGroup g = new RadioGroup(mC);
                        for (String[] ch : s.choices) {
                            RadioButton rb = new RadioButton(mC);
                            rb.setId(View.generateViewId());
                            rb.setText(ch[1]);
                            rb.setTextColor(mC.getColor(R.color.abyss_text));
                            rb.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16);
                            g.addView(rb);
                            f.groupValues.add(ch[0]);
                            if (ch[0].equals(v)) {
                                rb.setChecked(true);
                            }
                        }
                        g.setOnCheckedChangeListener((gr, id) -> changed());
                        parent.addView(g, lp(mC, 4));
                        f.group = g;
                        break;
                    }
                    case "secret": {
                        anySecret = true;
                        EditText e = new EditText(mC);
                        e.setInputType(InputType.TYPE_CLASS_TEXT
                                | InputType.TYPE_TEXT_VARIATION_PASSWORD);
                        e.setHint(R.string.mod_secret_not_set);
                        e.setEnabled(false);
                        e.setTextColor(mC.getColor(R.color.abyss_text));
                        e.setHintTextColor(mC.getColor(R.color.abyss_dim));
                        parent.addView(e, lp(mC, 4));
                        dim(parent, mC.getString(R.string.mod_secret_later));
                        break;
                    }
                    default: {
                        EditText e = new EditText(mC);
                        e.setSingleLine(true);
                        e.setTextColor(mC.getColor(R.color.abyss_text));
                        e.setHintTextColor(mC.getColor(R.color.abyss_dim));
                        if ("number".equals(s.type)) {
                            int t = InputType.TYPE_CLASS_NUMBER;
                            if (!s.integer) {
                                t |= InputType.TYPE_NUMBER_FLAG_DECIMAL;
                            }
                            if (s.min == null || s.min < 0) {
                                t |= InputType.TYPE_NUMBER_FLAG_SIGNED;
                            }
                            e.setInputType(t);
                            if (s.min != null || s.max != null) {
                                e.setHint(ModuleSpec.fmt(s.min) + " – " + ModuleSpec.fmt(s.max));
                            }
                        } else {
                            String d = s.def instanceof String ? (String) s.def : "";
                            e.setInputType(InputType.TYPE_CLASS_TEXT | (d.contains("://")
                                    ? InputType.TYPE_TEXT_VARIATION_URI : 0));
                            if (!d.isEmpty()) {
                                e.setHint(d);
                            }
                        }
                        if (v != null) {
                            e.setText(s.envValue(v));
                        }
                        e.addTextChangedListener(new TextWatcher() {
                            @Override
                            public void beforeTextChanged(CharSequence a, int b, int c, int d) {
                            }

                            @Override
                            public void onTextChanged(CharSequence a, int b, int c, int d) {
                            }

                            @Override
                            public void afterTextChanged(Editable ed) {
                                changed();
                            }
                        });
                        parent.addView(e, lp(mC, 4));
                        f.edit = e;
                    }
                }
                f.error = warn(parent, "");
                f.error.setVisibility(View.GONE);
                mFields.add(f);
            }
            if (anySecret) {
                dim(parent, mC.getString(R.string.mod_secret_honesty));
            }
        }

        void setOnChange(Runnable r) {
            mOnChange = r;
        }

        private void changed() {
            if (mOnChange != null) {
                mOnChange.run();
            }
        }

        /** The values on screen by key (non-secrets; empty fields left out). */
        Map<String, Object> values() {
            Map<String, Object> out = new LinkedHashMap<>();
            for (Field f : mFields) {
                Object v = null;
                if (f.toggle != null) {
                    v = f.toggle.isChecked();
                } else if (f.group != null) {
                    int i = f.group.indexOfChild(f.group.findViewById(
                            f.group.getCheckedRadioButtonId()));
                    v = i >= 0 ? f.groupValues.get(i) : null;
                } else if (f.edit != null) {
                    v = f.s.parse(f.edit.getText().toString());
                }
                if (v != null) {
                    out.put(f.s.key, v);
                }
            }
            return out;
        }

        /** Shows each field's problem; returns true when everything is acceptable. */
        boolean validate() {
            Map<String, Object> vals = values();
            boolean ok = true;
            for (Field f : mFields) {
                if (f.s.isSecret()) {
                    continue;
                }
                Object v = vals.containsKey(f.s.key) ? vals.get(f.s.key) : f.s.def;
                String bad = f.s.check(v);
                f.error.setText(bad == null ? "" : bad);
                f.error.setVisibility(bad == null ? View.GONE : View.VISIBLE);
                ok &= bad == null;
            }
            return ok;
        }
    }
}
