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
import android.content.Intent;
import android.media.AudioDeviceCallback;
import android.media.AudioDeviceInfo;
import android.media.AudioManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemProperties;
import android.util.Log;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;

/**
 * The Abyss control center (main-ROM side), top page. Opened from the launcher and from the
 * Settings home page tile ("Abyss", injected with com.android.settings.action.IA_SETTINGS, see
 * the manifest). Short on purpose:
 * <ul>
 * <li><b>State + Enter</b>: what the first ● plays, where the sound goes, the battery
 *     ({@link Status}); "Enter Abyss": {@link GoDark}, then sys.abyss.request=enter; init
 *     (abyss.rc) stops the framework, which ends this process. One line of what stops and a
 *     link to the full pre-flight text ("Before you enter").</li>
 * <li><b>Quick</b>: the settings people change between sessions ({@link Sections#quick}).</li>
 * <li><b>Apps</b>: Music and each installed module, each with its own page (settings,
 *     captures, permissions, update/remove), and Install apps ({@link Sections#apps}).</li>
 * <li><b>System</b>: Bluetooth &amp; audio, Internet, Navigation &amp; keys, Screen &amp;
 *     power, Advanced ({@link Sections#SYSTEM}), each opening {@link SectionActivity}.</li>
 * </ul>
 * Rows are added in {@link Sections} only (see the comment at its top); this class owns the
 * Enter flow. Property protocol: device/samsung/sm7325-common/abyss/INTEGRATION.md. SELinux:
 * abyss/sepolicy/private/abyss_entry.te.
 */
public final class EntryActivity extends Activity {
    private static final String TAG = "AbyssEntry";

    static final String PROP_REQUEST = "sys.abyss.request";
    static final String PROP_STATE = "sys.abyss.state";
    private static final String STATE_OFF = "off";

    /** If init has not picked the request up by then, say so and allow another try. */
    private static final long ENTER_TIMEOUT_MS = 5000;

    private final Handler mHandler = new Handler(Looper.getMainLooper());

    private Page mPage;
    private AudioManager mAudioManager;
    private Button mEnter;
    private TextView mStatus;
    private TextView mPlays;
    private TextView mOutput;
    private TextView mBattery;
    private boolean mEntering;

    private final AudioDeviceCallback mDeviceCallback = new AudioDeviceCallback() {
        @Override
        public void onAudioDevicesAdded(AudioDeviceInfo[] added) {
            refreshState();
        }

        @Override
        public void onAudioDevicesRemoved(AudioDeviceInfo[] removed) {
            refreshState();
        }
    };

    private final Runnable mEnterTimeout = () -> {
        mEntering = false;
        if (STATE_OFF.equals(SystemProperties.get(PROP_STATE, ""))) {
            showStatus(getString(R.string.failed));
        }
        refreshEnter();
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        mAudioManager = getSystemService(AudioManager.class);

        mPage = new Page(this, getString(R.string.title), false);
        mPage.intro(getString(R.string.intro));
        buildEnter(mPage.group(0));
        Sections.quick(mPage);
        Sections.apps(mPage);
        Sections.system(mPage);
    }

    @Override
    protected void onResume() {
        super.onResume();
        mAudioManager.registerAudioDeviceCallback(mDeviceCallback, mHandler);
        refreshEnter();
        mPage.refresh();
    }

    @Override
    protected void onPause() {
        mAudioManager.unregisterAudioDeviceCallback(mDeviceCallback);
        super.onPause();
    }

    @Override
    protected void onDestroy() {
        mHandler.removeCallbacks(mEnterTimeout);
        super.onDestroy();
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        mPage.onActivityResult(requestCode, resultCode, data);
    }

    // ---- State + Enter -----------------------------------------------------------------------

    private void buildEnter(Page.Group g) {
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        mPlays = stateLine(col, R.string.state_plays_label, 0);
        mOutput = stateLine(col, R.string.state_out_label, 8);
        mBattery = stateLine(col, R.string.state_battery_label, 8);

        mStatus = mPage.text("", 15, R.color.abyss_warn, mPage.bodyFace(false));
        mStatus.setVisibility(View.GONE);
        col.addView(mStatus, mPage.lp(12));

        // Button without the platform button style: the page's filled accent pill.
        mEnter = new Button(this, null, 0, 0);
        mEnter.setText(R.string.enter);
        mEnter.setAllCaps(false);
        mEnter.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
        mEnter.setTypeface(mPage.bodyFace(true));
        mEnter.setGravity(Gravity.CENTER);
        mEnter.setMinHeight(mPage.dp(56));
        mEnter.setBackground(mPage.primaryDrawable());
        mEnter.setOnClickListener(v -> enter());
        col.addView(mEnter, mPage.lp(16));

        TextView note = mPage.summary(getString(R.string.enter_note));
        note.setGravity(Gravity.CENTER_HORIZONTAL);
        col.addView(note, mPage.lp(10));
        // The full pre-flight text: what stops, what keeps going, coming back.
        TextView more = mPage.text(getString(R.string.enter_more), 14, R.color.abyss_accent,
                mPage.bodyFace(true));
        more.setGravity(Gravity.CENTER);
        more.setMinHeight(mPage.dp(44));
        more.setClickable(true);
        more.setFocusable(true);
        TypedValue tv = new TypedValue();
        getTheme().resolveAttribute(android.R.attr.selectableItemBackground, tv, true);
        more.setBackgroundResource(tv.resourceId);
        more.setOnClickListener(v -> mPage.start(SectionActivity.intent(this, "before")));
        col.addView(more, mPage.lp(2));
        g.view(col);
        mPage.onRefresh(this::refreshState);
    }

    /** A "label  value" line of the state block; returns the value view. */
    private TextView stateLine(LinearLayout col, int label, int topDp) {
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        TextView l = mPage.text(getString(label), 14, R.color.abyss_dim, mPage.bodyFace(false));
        l.setMinWidth(mPage.dp(76));
        row.addView(l, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT));
        TextView v = mPage.text("", 15, R.color.abyss_text, mPage.bodyFace(true));
        row.addView(v, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
        col.addView(row, mPage.lp(topDp));
        return v;
    }

    private void refreshState() {
        mPlays.setText(Status.plays(this));
        String out = Status.output(this, mAudioManager);
        mOutput.setText(out != null ? out : getString(R.string.state_out_none));
        mOutput.setTextColor(getColor(out != null ? R.color.abyss_text : R.color.abyss_warn));
        mBattery.setText(Status.battery(this));
    }

    private void refreshEnter() {
        if (mEntering) {
            return;
        }
        String state = SystemProperties.get(PROP_STATE, "");
        if (state.isEmpty()) {
            // abyss.rc sets "off" on boot; no value means this build has no Abyss.
            showStatus(getString(R.string.not_installed));
            setButton(false, R.string.enter);
        } else if (!STATE_OFF.equals(state)) {
            showStatus(getString(R.string.busy, state));
            setButton(false, R.string.enter);
        } else {
            mStatus.setVisibility(View.GONE);
            setButton(true, R.string.enter);
        }
        refreshState();
    }

    private void enter() {
        if (mEntering || !STATE_OFF.equals(SystemProperties.get(PROP_STATE, ""))) {
            refreshEnter();
            return;
        }
        mEntering = true;
        setButton(false, R.string.going);
        mStatus.setVisibility(View.GONE);
        // Radios and screen-off touch gestures off while system_server can still apply it;
        // BootReceiver puts them back after the return.
        GoDark.apply(this);
        try {
            SystemProperties.set(PROP_REQUEST, "enter");
        } catch (RuntimeException e) {
            // SELinux denial or property service error: sys.abyss.request is settable by
            // system_app only through abyss/sepolicy/private/abyss_entry.te.
            Log.e(TAG, "setprop " + PROP_REQUEST + " failed", e);
            mEntering = false;
            showStatus(getString(R.string.failed));
            refreshEnter();
            return;
        }
        Log.i(TAG, "requested enter");
        mHandler.postDelayed(mEnterTimeout, ENTER_TIMEOUT_MS);
    }

    private void showStatus(String text) {
        mStatus.setText(text);
        mStatus.setVisibility(View.VISIBLE);
    }

    private void setButton(boolean enabled, int textRes) {
        mEnter.setEnabled(enabled);
        mEnter.setText(textRes);
        mEnter.setTextColor(getColor(enabled ? R.color.abyss_on_accent : R.color.abyss_dim));
    }
}
