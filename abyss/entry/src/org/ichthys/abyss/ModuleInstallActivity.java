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
import android.database.Cursor;
import android.net.Uri;
import android.os.Bundle;
import android.provider.OpenableColumns;
import android.util.Log;
import android.widget.LinearLayout;
import android.widget.Toast;

import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * The install sheet for a picked .abyssapp: verify the signature and every file → show what
 * the module can do and its settings → a person holds "Hold to install" → the module is written
 * to /data/misc/abyss/store/&lt;id&gt;/ and its settings to conf/settings/app-&lt;id&gt;.env
 * (ModuleStore, atomic). Not exported: only ModulesActivity's own picker result reaches it, so
 * no other app can push an install sheet.
 *
 * <p>Trust: a file install has no pinned source key. The signer's key is shown; the first
 * install pins it (install.json), and an update signed by another key is refused until the
 * installed module is removed. A signature only says who made the file, not that it is safe:
 * the sheet says what the module can do in plain words.
 */
public final class ModuleInstallActivity extends Activity {
    private static final String TAG = "AbyssModules";

    private LinearLayout mCol;
    private ModulePackage mPkg;
    private ModuleSpec mSpec;
    private ModuleStore.Installed mOld;
    private String mFileName;
    private ModuleUi.SettingsForm mForm;
    private LinearLayout mPerms;
    private boolean mBusy;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        mCol = ModuleUi.page(this, true);
        ModuleUi.title(mCol, getString(R.string.mod_checking));
        Uri uri = getIntent().getData();
        if (uri == null) {
            finish();
            return;
        }
        mFileName = displayName(uri);
        new Thread(() -> load(uri), "abyss-module-check").start();
    }

    private String displayName(Uri uri) {
        try (Cursor c = getContentResolver().query(uri,
                new String[] {OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (c != null && c.moveToFirst()) {
                return c.getString(0);
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "display name of " + uri, e);
        }
        return null;
    }

    private void load(Uri uri) {
        String refusal;
        try (InputStream in = getContentResolver().openInputStream(uri)) {
            if (in == null) {
                throw new IOException("no stream");
            }
            ModulePackage pkg = ModulePackage.read(in);
            ModuleSpec spec = ModuleSpec.parse(pkg, ModuleSpec.servedFeatures());
            ModuleStore.Installed old = new ModuleStore(new File(ModuleStore.ROOT)).get(spec.id);
            runOnUiThread(() -> show(pkg, spec, old));
            return;
        } catch (ModulePackage.Refused e) {
            refusal = e.getMessage();
        } catch (ModuleSpec.Invalid e) {
            refusal = getString(R.string.mod_invalid, e.getMessage());
        } catch (IOException | RuntimeException e) {
            Log.w(TAG, "read " + uri, e);
            refusal = getString(R.string.mod_unreadable);
        }
        String r = refusal;
        runOnUiThread(() -> refuse(r));
    }

    private void refuse(String why) {
        mCol.removeAllViews();
        ModuleUi.title(mCol, getString(R.string.mod_cant_install));
        if (mFileName != null) {
            ModuleUi.dim(mCol, mFileName);
        }
        ModuleUi.body(mCol, why);
        ModuleUi.button(mCol, getString(R.string.mod_close), v -> finish(), 28);
    }

    private void show(ModulePackage pkg, ModuleSpec spec, ModuleStore.Installed old) {
        mPkg = pkg;
        mSpec = spec;
        mOld = old;
        mCol.removeAllViews();
        ModuleUi.title(mCol, spec.name);
        ModuleUi.dim(mCol, getString(R.string.mod_sheet_sub, spec.version, spec.category,
                spec.authors.isEmpty() ? "?" : String.join(", ", spec.authors)));
        ModuleUi.body(mCol, spec.about);
        if (old != null) {
            int cmp = compareVersions(spec.version, old.version());
            String line = cmp > 0 ? getString(R.string.mod_update_from, old.version(),
                    spec.version) : cmp == 0 ? getString(R.string.mod_reinstall, old.version())
                    : getString(R.string.mod_downgrade, old.version(), spec.version);
            if (cmp < 0) {
                ModuleUi.warn(mCol, line);
            } else {
                ModuleUi.dim(mCol, line);
            }
        }

        // Who signed it.
        ModuleUi.section(mCol, getString(R.string.mod_sec_signed));
        LinearLayout sc = ModuleUi.card(mCol);
        ModuleUi.body(sc, pkg.signer.fingerprint).setTextIsSelectable(true);
        ModuleUi.dim(sc, pkg.signer.keyType + " · " + getString(R.string.mod_files_checked,
                pkg.files.size()));
        boolean keyChanged = old != null && old.fingerprint() != null
                && !old.fingerprint().equals(pkg.signer.fingerprint);
        if (keyChanged) {
            ModuleUi.warn(sc, getString(R.string.mod_key_changed, old.fingerprint()));
        } else if (old != null) {
            ModuleUi.dim(sc, getString(R.string.mod_key_same));
        } else {
            ModuleUi.dim(sc, getString(R.string.mod_key_new));
        }

        // What it can do (follows the settings below: the server address is a setting).
        ModuleUi.section(mCol, getString(R.string.mod_sec_can_do));
        mPerms = ModuleUi.card(mCol);

        ModuleUi.section(mCol, getString(R.string.mod_sec_settings));
        LinearLayout set = ModuleUi.card(mCol);
        Map<String, Object> start = old != null
                ? new ModuleStore(new File(ModuleStore.ROOT)).readSettings(spec)
                : new LinkedHashMap<>();
        mForm = new ModuleUi.SettingsForm(set, spec, start);
        mForm.setOnChange(this::showPermissions);
        ModuleUi.dim(set, getString(R.string.mod_settings_when));
        showPermissions();

        ModuleUi.dim(mCol, getString(R.string.mod_honesty)).setPadding(0, ModuleUi.dp(this,
                12), 0, 0);
        ModuleUi.HoldToApprove h = ModuleUi.hold(mCol, getString(old == null
                ? R.string.mod_hold_install : R.string.mod_hold_update), this::install);
        if (keyChanged) {
            h.setEnabled(false);
            h.setAlpha(0.4f);
        }
        ModuleUi.button(mCol, getString(R.string.cancel), v -> finish(), 12);
    }

    private void showPermissions() {
        if (mPerms == null) {
            return;
        }
        mPerms.removeAllViews();
        Map<String, Object> vals = mForm == null ? new LinkedHashMap<>() : mForm.values();
        boolean first = true;
        for (ModuleSpec.Permission p : mSpec.permissions(vals)) {
            ModuleUi.body(mPerms, "· " + p.words).setPadding(0, first ? 0 : ModuleUi.dp(this,
                    4), 0, 0);
            first = false;
        }
    }

    private void install() {
        if (mBusy || !mForm.validate()) {
            return;
        }
        mBusy = true;
        Map<String, Object> vals = mForm.values();
        new Thread(() -> {
            boolean ok;
            try {
                new ModuleStore(new File(ModuleStore.ROOT)).install(mPkg, mSpec, vals,
                        "touch", mFileName, System.currentTimeMillis());
                ok = true;
            } catch (IOException | RuntimeException e) {
                Log.w(TAG, "install " + mSpec.id, e);
                ok = false;
            }
            boolean done = ok;
            runOnUiThread(() -> {
                mBusy = false;
                if (done) {
                    Toast.makeText(this, getString(R.string.mod_installed, mSpec.name),
                            Toast.LENGTH_LONG).show();
                    setResult(RESULT_OK);
                    finish();
                } else {
                    Toast.makeText(this, R.string.mod_install_failed, Toast.LENGTH_LONG).show();
                }
            });
        }, "abyss-module-install").start();
    }

    /** semver-ish: numeric major.minor.patch, pre-release sorts before the release. */
    static int compareVersions(String a, String b) {
        String[] x = a.split("[-+]", 2)[0].split("\\.");
        String[] y = b.split("[-+]", 2)[0].split("\\.");
        for (int i = 0; i < 3; i++) {
            long p = i < x.length ? parse(x[i]) : 0;
            long q = i < y.length ? parse(y[i]) : 0;
            if (p != q) {
                return Long.compare(p, q);
            }
        }
        boolean pa = a.contains("-");
        boolean pb = b.contains("-");
        return pa == pb ? a.compareTo(b) : pa ? -1 : 1;
    }

    private static long parse(String s) {
        try {
            return Long.parseLong(s);
        } catch (NumberFormatException e) {
            return 0;
        }
    }
}
