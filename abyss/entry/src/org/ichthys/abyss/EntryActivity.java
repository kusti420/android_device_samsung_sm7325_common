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
import android.app.AlertDialog;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothClass;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothManager;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Intent;
import android.media.AudioDeviceCallback;
import android.media.AudioDeviceInfo;
import android.media.AudioManager;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.ParcelUuid;
import android.os.SystemProperties;
import android.provider.Settings;
import android.text.TextUtils;
import android.text.format.DateUtils;
import android.util.Log;
import android.util.TypedValue;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.SeekBar;
import android.widget.Switch;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONObject;

import java.io.File;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/**
 * The Abyss control center (main-ROM side). Opened from the launcher and from the Settings home
 * page tile ("Abyss", injected with com.android.settings.action.IA_SETTINGS, see the manifest).
 * Sections:
 * <ul>
 * <li><b>Enter</b>: honest pre-flight text and "Enter Abyss": {@link GoDark}, then
 *     sys.abyss.request=enter; init (abyss.rc) stops the framework, which ends this process.</li>
 * <li><b>Captures</b>: what abyssd saved on Volume Down + Power ({@link Captures}).</li>
 * <li><b>Settings</b>: persist.abyss.brightness, persist.abyss.screen_timeout_s,
 *     persist.abyss.nav.start (where Abyss opens), persist.abyss.suspend (abyssd reads them on entry), the volume limit (information only)
 *     and the paired Bluetooth headphones abyss-btd will offer.</li>
 * <li><b>Scripts</b> (v1.1) and <b>Apps &amp; containers</b> (v1.2): "coming next" cards only.</li>
 * </ul>
 * Property protocol: device/samsung/sm7325-common/abyss/INTEGRATION.md. SELinux:
 * abyss/sepolicy/private/abyss_entry.te.
 */
public final class EntryActivity extends Activity {
    private static final String TAG = "AbyssEntry";

    private static final String PROP_REQUEST = "sys.abyss.request";
    private static final String PROP_STATE = "sys.abyss.state";
    private static final String STATE_OFF = "off";

    /** Backlight level abyssd asks for (raw panel0-backlight units, max 486 on a52sxq). */
    private static final String PROP_BRIGHTNESS = "persist.abyss.brightness";
    private static final int BRIGHTNESS_MIN = 1;
    private static final int BRIGHTNESS_MAX = 486;
    private static final int BRIGHTNESS_DEFAULT = 160;

    /** Seconds the screen stays on after a key press (abyssd clamps to 5..120). */
    private static final String PROP_SCREEN_TIMEOUT = "persist.abyss.screen_timeout_s";
    private static final int[] SCREEN_TIMEOUT_CHOICES = {10, 15, 30, 60};
    private static final int SCREEN_TIMEOUT_DEFAULT = 15;
    private static final int SCREEN_TIMEOUT_MIN = 5;
    private static final int SCREEN_TIMEOUT_MAX = 120;

    /** Where Abyss opens: "player" (default: last song ready, paused) or "home" (abyssd ui.rs). */
    private static final String PROP_NAV_START = "persist.abyss.nav.start";
    private static final String[] NAV_START_VALUES = {"player", "home"};
    private static final int[] NAV_START_LABELS = {R.string.nav_start_player, R.string.nav_start_home};
    /** Internet in Abyss (abyssd uplink.rs); first value = default. */
    private static final String PROP_NET_MODE = "persist.abyss.net.mode";
    private static final String[] NET_MODE_VALUES = {"wifi_mobile", "wifi", "mobile"};
    private static final int[] NET_MODE_LABELS =
            {R.string.net_mode_wifi_mobile, R.string.net_mode_wifi, R.string.net_mode_mobile};
    private static final String PROP_NET_FORMAT = "persist.abyss.net.mobile_format";
    private static final String[] NET_FORMAT_VALUES = {"mp3-128", "same"};
    private static final int[] NET_FORMAT_LABELS = {R.string.net_format_small, R.string.net_format_same};

    /** Experimental kernel suspend while idle (abyssd suspend.rs), off by default. */
    private static final String PROP_SUSPEND = "persist.abyss.suspend";

    /** abyssd's last Bluetooth headphones: {"addr": "...", "name": "..."} (bt.rs LAST_FILE). */
    private static final String BT_LAST_FILE = Captures.DIR + "/bt_last.json";
    /** A2DP sink (headphones / speaker) service class UUID. */
    private static final ParcelUuid A2DP_SINK =
            ParcelUuid.fromString("0000110B-0000-1000-8000-00805F9B34FB");

    /** If init has not picked the request up by then, say so and allow another try. */
    private static final long ENTER_TIMEOUT_MS = 5000;
    private static final int REQ_SAVE_FILE = 1;

    private final Handler mHandler = new Handler(Looper.getMainLooper());

    private AudioManager mAudioManager;
    private Button mEnter;
    private TextView mStatus;
    private TextView mHeadphones;
    private boolean mEntering;

    private TextView mCapturesHeader;
    private TextView mCapturesInfo;
    private LinearLayout mCapturesList;
    private View mCapturesActions;
    private List<Captures.Item> mCaptures = new ArrayList<>();

    private SeekBar mBrightness;
    private TextView mBrightnessValue;
    private boolean mBrightnessTracking;
    private LinearLayout mScreenTimeout;
    private TextView mScreenTimeoutOther;
    private LinearLayout mNavStart;
    private LinearLayout mNetMode;
    private LinearLayout mNetFormat;
    private Switch mSuspend;
    private boolean mBinding;
    private LinearLayout mBtList;
    private TextView mSettingsError;

    private final AudioDeviceCallback mDeviceCallback = new AudioDeviceCallback() {
        @Override
        public void onAudioDevicesAdded(AudioDeviceInfo[] added) {
            updateHeadphones();
        }

        @Override
        public void onAudioDevicesRemoved(AudioDeviceInfo[] removed) {
            updateHeadphones();
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
        setContentView(R.layout.center);
        mAudioManager = getSystemService(AudioManager.class);

        mEnter = findViewById(R.id.enter);
        mStatus = findViewById(R.id.status);
        mHeadphones = findViewById(R.id.headphones);
        mEnter.setOnClickListener(v -> enter());

        mCapturesHeader = findViewById(R.id.captures_header);
        mCapturesInfo = findViewById(R.id.captures_info);
        mCapturesList = findViewById(R.id.captures_list);
        mCapturesActions = findViewById(R.id.captures_actions);
        findViewById(R.id.captures_share_all).setOnClickListener(v -> shareText(
                Captures.asText(mCaptures)));
        findViewById(R.id.captures_save_file).setOnClickListener(v -> saveToFile());
        findViewById(R.id.captures_clear_all).setOnClickListener(v -> confirmClearAll());

        setUpSettings();

        setUpComingNext(R.id.scripts_tag, "v1.1", R.id.docs_scripts, R.string.docs_path_scripts);
        setUpComingNext(R.id.apps_tag, "v1.2", R.id.docs_apps, R.string.docs_path_apps);
        // Modules (impl-radio-module): Settings › Abyss › Modules.
        findViewById(R.id.modules_open).setOnClickListener(
                v -> startActivity(new Intent(this, ModulesActivity.class)));
    }

    @Override
    protected void onResume() {
        super.onResume();
        mAudioManager.registerAudioDeviceCallback(mDeviceCallback, mHandler);
        refreshEnter();
        refreshCaptures();
        refreshSettings();
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

    // ---- Enter -------------------------------------------------------------------------------

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
        updateHeadphones();
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

    private void updateHeadphones() {
        boolean wired = false;
        for (AudioDeviceInfo d : mAudioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS)) {
            int t = d.getType();
            if (t == AudioDeviceInfo.TYPE_WIRED_HEADPHONES
                    || t == AudioDeviceInfo.TYPE_WIRED_HEADSET
                    || t == AudioDeviceInfo.TYPE_USB_HEADSET
                    || t == AudioDeviceInfo.TYPE_USB_DEVICE) {
                wired = true;
                break;
            }
        }
        mHeadphones.setVisibility(wired ? View.GONE : View.VISIBLE);
    }

    private void showStatus(String text) {
        mStatus.setText(text);
        mStatus.setVisibility(View.VISIBLE);
    }

    private void setButton(boolean enabled, int textRes) {
        mEnter.setEnabled(enabled);
        mEnter.setText(textRes);
        mEnter.setTextColor(getColor(enabled ? R.color.abyss_accent : R.color.abyss_dim));
    }

    // ---- Captures ----------------------------------------------------------------------------

    private void refreshCaptures() {
        Captures.Loaded loaded = Captures.load();
        mCaptures = loaded.items;
        mCapturesList.removeAllViews();
        int n = mCaptures.size();
        mCapturesHeader.setText(n == 0 ? getString(R.string.sec_captures)
                : getString(R.string.sec_captures) + " · " + getString(R.string.captures_count, n));
        if (loaded.error) {
            mCapturesInfo.setText(R.string.captures_unreadable);
            mCapturesInfo.setTextColor(getColor(R.color.abyss_warn));
        } else {
            mCapturesInfo.setText(n == 0 ? R.string.captures_empty : R.string.captures_hint);
            mCapturesInfo.setTextColor(getColor(n == 0 ? R.color.abyss_text : R.color.abyss_dim));
        }
        mCapturesList.setVisibility(n == 0 ? View.GONE : View.VISIBLE);
        mCapturesActions.setVisibility(n == 0 ? View.GONE : View.VISIBLE);
        for (Captures.Item it : mCaptures) {
            mCapturesList.addView(captureRow(it));
        }
    }

    private View captureRow(Captures.Item it) {
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.VERTICAL);
        row.setBackgroundResource(R.drawable.row_bg);
        int pad = dp(10);
        row.setPadding(dp(4), pad, dp(4), pad);
        row.setClickable(true);
        row.setFocusable(true);

        TextView line = new TextView(this);
        line.setText(it.line);
        line.setTextColor(getColor(R.color.abyss_text));
        line.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16);
        row.addView(line);

        String meta = metaOf(it);
        if (!meta.isEmpty()) {
            TextView sub = new TextView(this);
            sub.setText(meta);
            sub.setTextColor(getColor(R.color.abyss_dim));
            sub.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
            sub.setPadding(0, dp(2), 0, 0);
            row.addView(sub);
        }

        row.setOnClickListener(v -> copy(it.line));
        row.setOnLongClickListener(v -> {
            captureActions(it);
            return true;
        });
        row.setContentDescription(it.line);
        return row;
    }

    private String metaOf(Captures.Item it) {
        List<String> parts = new ArrayList<>();
        if (!it.kind.isEmpty()) {
            parts.add(it.kind);
        }
        if (it.tsMs > 0) {
            parts.add(DateUtils.formatDateTime(this, it.tsMs, DateUtils.FORMAT_SHOW_DATE
                    | DateUtils.FORMAT_SHOW_TIME | DateUtils.FORMAT_ABBREV_MONTH));
        }
        return TextUtils.join(" · ", parts);
    }

    private void captureActions(Captures.Item it) {
        CharSequence[] items = {
                getString(R.string.capture_copy),
                getString(R.string.capture_share),
                getString(R.string.capture_delete),
        };
        new AlertDialog.Builder(this)
                .setTitle(it.line)
                .setItems(items, (d, which) -> {
                    if (which == 0) {
                        copy(it.line);
                    } else if (which == 1) {
                        shareText(it.line);
                    } else {
                        delete(it);
                    }
                })
                .setNegativeButton(R.string.cancel, null)
                .show();
    }

    private void copy(String text) {
        ClipboardManager cm = getSystemService(ClipboardManager.class);
        cm.setPrimaryClip(ClipData.newPlainText(getString(R.string.captures_share_subject), text));
        // Android 13+ shows its own "copied" confirmation; no toast on top of it.
    }

    private void shareText(String text) {
        Intent send = new Intent(Intent.ACTION_SEND)
                .setType("text/plain")
                .putExtra(Intent.EXTRA_TEXT, text)
                .putExtra(Intent.EXTRA_SUBJECT, getString(R.string.captures_share_subject));
        try {
            startActivity(Intent.createChooser(send, null));
        } catch (ActivityNotFoundException e) {
            Log.w(TAG, "no share target", e);
        }
    }

    private void delete(Captures.Item it) {
        List<Captures.Item> keep = new ArrayList<>(mCaptures);
        // Remove exactly this row (identity), not every line with the same text.
        for (int i = 0; i < keep.size(); i++) {
            if (keep.get(i) == it) {
                keep.remove(i);
                break;
            }
        }
        rewriteCaptures(keep, R.string.captures_deleted);
    }

    private void confirmClearAll() {
        new AlertDialog.Builder(this)
                .setTitle(R.string.captures_clear_title)
                .setMessage(getString(R.string.captures_clear_body, mCaptures.size()))
                .setPositiveButton(R.string.captures_clear_ok,
                        (d, w) -> rewriteCaptures(new ArrayList<>(), 0))
                .setNegativeButton(R.string.cancel, null)
                .show();
    }

    private void rewriteCaptures(List<Captures.Item> keep, int doneRes) {
        // Re-read first: the list on screen is what we change, but never drop lines that are
        // in the file and not on screen (there are none while Android runs; belt and braces).
        Captures.Loaded now = Captures.load();
        if (now.error || now.items.size() != mCaptures.size()) {
            refreshCaptures();
            Toast.makeText(this, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
            return;
        }
        if (Captures.rewrite(keep)) {
            if (doneRes != 0) {
                Toast.makeText(this, doneRes, Toast.LENGTH_SHORT).show();
            }
        } else {
            Toast.makeText(this, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
        }
        refreshCaptures();
    }

    /** Storage Access Framework: the user picks where the text file goes. */
    private void saveToFile() {
        Intent i = new Intent(Intent.ACTION_CREATE_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("text/plain")
                .putExtra(Intent.EXTRA_TITLE, getString(R.string.captures_file_name));
        try {
            startActivityForResult(i, REQ_SAVE_FILE);
        } catch (ActivityNotFoundException e) {
            Log.w(TAG, "no document picker", e);
            Toast.makeText(this, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != REQ_SAVE_FILE || resultCode != RESULT_OK || data == null
                || data.getData() == null) {
            return;
        }
        Uri uri = data.getData();
        List<Captures.Item> items = Captures.load().items;
        try (OutputStream out = getContentResolver().openOutputStream(uri, "wt")) {
            if (out == null) {
                throw new IOException("no stream for " + uri);
            }
            out.write(Captures.asText(items).getBytes(StandardCharsets.UTF_8));
            Toast.makeText(this, getString(R.string.captures_saved_to_file, items.size()),
                    Toast.LENGTH_SHORT).show();
        } catch (IOException | RuntimeException e) {
            Log.w(TAG, "save captures to " + uri, e);
            Toast.makeText(this, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
        }
    }

    // ---- Settings ----------------------------------------------------------------------------

    private void setUpSettings() {
        mSettingsError = findViewById(R.id.settings_error);

        mBrightness = findViewById(R.id.brightness);
        mBrightnessValue = findViewById(R.id.brightness_value);
        mBrightness.setMin(BRIGHTNESS_MIN);
        mBrightness.setMax(BRIGHTNESS_MAX);
        mBrightness.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
            @Override
            public void onProgressChanged(SeekBar s, int progress, boolean fromUser) {
                showBrightness(progress);
                // Keyboard / accessibility steps have no start/stop: save each one.
                if (fromUser && !mBrightnessTracking) {
                    setProp(PROP_BRIGHTNESS, Integer.toString(progress));
                }
            }

            @Override
            public void onStartTrackingTouch(SeekBar s) {
                mBrightnessTracking = true;
            }

            @Override
            public void onStopTrackingTouch(SeekBar s) {
                mBrightnessTracking = false;
                setProp(PROP_BRIGHTNESS, Integer.toString(s.getProgress()));
            }
        });

        mScreenTimeout = findViewById(R.id.screen_timeout);
        mScreenTimeoutOther = findViewById(R.id.screen_timeout_other);
        for (int i = 0; i < SCREEN_TIMEOUT_CHOICES.length; i++) {
            int secs = SCREEN_TIMEOUT_CHOICES[i];
            TextView chip = new TextView(this, null, 0, R.style.AbyssChip);
            LinearLayout.LayoutParams lp =
                    new LinearLayout.LayoutParams(0, dp(44), 1f);
            if (i > 0) {
                lp.setMarginStart(dp(8));
            }
            chip.setLayoutParams(lp);
            chip.setText(getString(R.string.seconds, secs));
            chip.setTag(secs);
            chip.setOnClickListener(v -> {
                if (setProp(PROP_SCREEN_TIMEOUT, Integer.toString(secs))) {
                    showScreenTimeout(secs);
                }
            });
            mScreenTimeout.addView(chip);
        }

        mNavStart = findViewById(R.id.nav_start);
        addChoices(mNavStart, PROP_NAV_START, NAV_START_VALUES, NAV_START_LABELS);
        mNetMode = findViewById(R.id.net_mode);
        addChoices(mNetMode, PROP_NET_MODE, NET_MODE_VALUES, NET_MODE_LABELS);
        mNetFormat = findViewById(R.id.net_format);
        addChoices(mNetFormat, PROP_NET_FORMAT, NET_FORMAT_VALUES, NET_FORMAT_LABELS);

        mSuspend = findViewById(R.id.suspend);
        mSuspend.setOnCheckedChangeListener((b, on) -> {
            if (mBinding) {
                return;
            }
            if (!setProp(PROP_SUSPEND, on ? "1" : "0")) {
                mBinding = true;
                b.setChecked(!on);
                mBinding = false;
            }
        });
        findViewById(R.id.suspend_row).setOnClickListener(v -> mSuspend.toggle());

        mBtList = findViewById(R.id.bt_list);
        findViewById(R.id.bt_open_settings).setOnClickListener(v -> {
            try {
                startActivity(new Intent(Settings.ACTION_BLUETOOTH_SETTINGS));
            } catch (ActivityNotFoundException e) {
                Log.w(TAG, "no Bluetooth settings", e);
            }
        });
    }

    private void refreshSettings() {
        mBinding = true;
        int level = clamp(SystemProperties.getInt(PROP_BRIGHTNESS, BRIGHTNESS_DEFAULT),
                BRIGHTNESS_MIN, BRIGHTNESS_MAX);
        mBrightness.setProgress(level);
        showBrightness(level);
        int secs = SystemProperties.getInt(PROP_SCREEN_TIMEOUT, SCREEN_TIMEOUT_DEFAULT);
        if (secs < SCREEN_TIMEOUT_MIN || secs > SCREEN_TIMEOUT_MAX) {
            secs = SCREEN_TIMEOUT_DEFAULT;
        }
        showScreenTimeout(secs);
        showChoice(mNavStart, SystemProperties.get(PROP_NAV_START, ""), NAV_START_VALUES);
        showChoice(mNetMode, SystemProperties.get(PROP_NET_MODE, ""), NET_MODE_VALUES);
        showChoice(mNetFormat, SystemProperties.get(PROP_NET_FORMAT, ""), NET_FORMAT_VALUES);
        mSuspend.setChecked(SystemProperties.getBoolean(PROP_SUSPEND, false));
        mBinding = false;
        refreshBluetooth();
    }

    private void showBrightness(int level) {
        mBrightnessValue.setText(level == BRIGHTNESS_DEFAULT
                ? getString(R.string.brightness_default, level)
                : getString(R.string.brightness_value, level));
    }

    private void showScreenTimeout(int secs) {
        boolean matched = false;
        for (int i = 0; i < mScreenTimeout.getChildCount(); i++) {
            View chip = mScreenTimeout.getChildAt(i);
            boolean sel = Integer.valueOf(secs).equals(chip.getTag());
            matched |= sel;
            chip.setSelected(sel);
            ((TextView) chip).setTextColor(getColor(sel ? R.color.abyss_accent : R.color.abyss_text));
        }
        // A value set from a shell (e.g. 20 s) is shown as it is, not silently replaced.
        mScreenTimeoutOther.setVisibility(matched ? View.GONE : View.VISIBLE);
        if (!matched) {
            mScreenTimeoutOther.setText(getString(R.string.screen_timeout_other, secs));
        }
    }

    /** One chip per value; a tap writes it to {@code prop}. The first value is the default. */
    private void addChoices(LinearLayout row, String prop, String[] values, int[] labels) {
        for (int i = 0; i < values.length; i++) {
            String value = values[i];
            TextView chip = new TextView(this, null, 0, R.style.AbyssChip);
            LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(0, dp(44), 1f);
            if (i > 0) {
                lp.setMarginStart(dp(8));
            }
            chip.setLayoutParams(lp);
            chip.setText(labels[i]);
            chip.setTag(value);
            chip.setOnClickListener(v -> {
                if (setProp(prop, value)) {
                    showChoice(row, value, values);
                }
            });
            row.addView(chip);
        }
    }

    /** Marks the chip of {@code current}; unset or unknown = the default (first) value. */
    private void showChoice(LinearLayout row, String current, String[] values) {
        String shown = values[0];
        for (String v : values) {
            if (v.equalsIgnoreCase(current.trim())) {
                shown = v;
            }
        }
        for (int i = 0; i < row.getChildCount(); i++) {
            View chip = row.getChildAt(i);
            boolean sel = shown.equals(chip.getTag());
            chip.setSelected(sel);
            ((TextView) chip).setTextColor(getColor(sel ? R.color.abyss_accent : R.color.abyss_text));
        }
    }

    /** persist.abyss.* = abyss_config_prop, settable by system_app (abyss_entry.te). */
    private boolean setProp(String name, String value) {
        try {
            SystemProperties.set(name, value);
            mSettingsError.setVisibility(View.GONE);
            return true;
        } catch (RuntimeException e) {
            Log.e(TAG, "setprop " + name + " failed", e);
            mSettingsError.setText(R.string.setting_failed);
            mSettingsError.setVisibility(View.VISIBLE);
            return false;
        }
    }

    /**
     * Android's paired audio devices: abyss-btd runs the same stack on the same bonding store
     * (/data/misc/bluedroid), so this is the list Abyss offers. "Last used" is abyssd's
     * out/bt_last.json; it is shown even while Android's Bluetooth is off.
     */
    private void refreshBluetooth() {
        mBtList.removeAllViews();
        String[] last = readBtLast();
        boolean lastShown = false;
        BluetoothManager bm = getSystemService(BluetoothManager.class);
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
            mBtList.addView(btRow(nameOf(d), isLast));
        }
        if (last != null && !lastShown) {
            mBtList.addView(btRow(last[1].isEmpty() ? last[0] : last[1], true));
        }
        if (audio.isEmpty()) {
            TextView t = new TextView(this);
            t.setText(on ? R.string.bt_none : R.string.bt_off);
            t.setTextColor(getColor(R.color.abyss_dim));
            t.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
            t.setPadding(0, dp(6), 0, 0);
            mBtList.addView(t);
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

    private View btRow(String name, boolean last) {
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setBackgroundResource(R.drawable.row_bg);
        row.setPadding(dp(4), dp(10), dp(4), dp(10));

        TextView n = new TextView(this);
        n.setText(name);
        n.setTextColor(getColor(R.color.abyss_text));
        n.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16);
        n.setLayoutParams(new LinearLayout.LayoutParams(0,
                ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
        row.addView(n);

        if (last) {
            TextView tag = new TextView(this);
            tag.setText(R.string.bt_last_used);
            tag.setTextColor(getColor(R.color.abyss_accent));
            tag.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
            row.addView(tag);
        }
        return row;
    }

    /** {addr, name} from out/bt_last.json, or null. */
    private static String[] readBtLast() {
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

    // ---- Coming next -------------------------------------------------------------------------

    /**
     * "coming next" tag + "Read more" line. The line is a link only when docs_base_url is set
     * (strings.xml); until the docs are published it is plain dim text, never a dead link.
     */
    private void setUpComingNext(int tagId, String release, int docsId, int pathRes) {
        ((TextView) findViewById(tagId)).setText(getString(R.string.coming_next, release));
        TextView docs = findViewById(docsId);
        String base = getString(R.string.docs_base_url).trim();
        if (base.isEmpty()) {
            return;
        }
        Uri uri = Uri.parse(base.replaceAll("/+$", "") + "/" + getString(pathRes));
        docs.setTextColor(getColor(R.color.abyss_accent));
        docs.setClickable(true);
        docs.setOnClickListener(v -> {
            try {
                startActivity(new Intent(Intent.ACTION_VIEW, uri));
            } catch (ActivityNotFoundException e) {
                Log.w(TAG, "no browser for " + uri, e);
            }
        });
    }

    // ---- Helpers -----------------------------------------------------------------------------

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }

    private static int clamp(int v, int lo, int hi) {
        return Math.max(lo, Math.min(hi, v));
    }
}
