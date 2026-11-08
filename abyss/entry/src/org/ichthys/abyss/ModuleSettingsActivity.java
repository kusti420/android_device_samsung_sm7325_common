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
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.os.Bundle;
import android.util.Log;
import android.view.View;
import android.view.WindowManager;
import android.widget.LinearLayout;
import android.widget.Toast;

import java.io.File;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Settings › Abyss › ‹app›: everything about one installed module, on one page (the owner's
 * rule: each Abyss app's things on its own page). Its own settings (text, number, bool, enum;
 * secrets masked and write-only), the captures made while it played ({@link CapturesRows},
 * attributed by {@link Captures#owner}), what it can do and the addresses it may reach, its
 * version and signer, Update from file and Remove. Values are written to
 * /data/misc/abyss/conf/settings/app-&lt;id&gt;.env and used from the next entry into Abyss.
 *
 * <p>A setting that changes where the module may connect (a server address) needs the same
 * hold-to-approve as the install: the approved addresses are recorded in install.json and
 * abyssd only connects to those (contract §4). Other changes never need approval.
 * FLAG_SECURE: the page may show secret fields.
 */
public final class ModuleSettingsActivity extends Activity {
    static final String EXTRA_ID = "org.ichthys.abyss.extra.MODULE_ID";
    private static final String TAG = "AbyssModules";

    private Page mPage;
    private ModuleStore mStore;
    private ModuleStore.Installed mMod;
    private ModuleSpec mSpec;
    private ModuleUi.SettingsForm mForm;
    private LinearLayout mPerms;
    private LinearLayout mApprove;
    private View mSave;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_SECURE);
        mStore = new ModuleStore(new File(ModuleStore.ROOT));
        String id = getIntent().getStringExtra(EXTRA_ID);
        mMod = id == null ? null : mStore.get(id);
        if (mMod == null) {
            mPage = new Page(this, getString(R.string.mod_gone), true);
            return;
        }
        mPage = new Page(this, mMod.name(), true);
        String fp = mMod.fingerprint();
        mPage.intro(getString(R.string.mod_row_sub, mMod.version(),
                String.valueOf(mMod.manifest.get("category")),
                fp == null ? "?" : fp.substring(0, Math.min(fp.length(), 15)) + "…"));
        try {
            mSpec = mMod.spec();
        } catch (ModuleSpec.Invalid | RuntimeException e) {
            mPage.group(0).paragraph(0, getString(R.string.mod_broken, e.getMessage()))
                    .setTextColor(getColor(R.color.abyss_warn));
            actions();
            return;
        }
        String about = ModJson.str(mMod.manifest, "about");
        if (about != null) {
            mPage.intro(about);
        }

        // Its settings: the manifest's schema (ModuleUi.SettingsForm), saved with Save.
        LinearLayout set = column();
        mForm = new ModuleUi.SettingsForm(set, mSpec, mStore.readSettings(mSpec));
        mForm.setOnChange(this::refreshPermissions);
        ModuleUi.dim(set, getString(R.string.mod_settings_when));
        mApprove = new LinearLayout(this);
        mApprove.setOrientation(LinearLayout.VERTICAL);
        set.addView(mApprove, ModuleUi.lp(this, 0));
        mSave = ModuleUi.button(set, getString(R.string.mod_save), v -> save(false), 16);
        mPage.group(R.string.mod_sec_settings).view(set);

        // Songs saved while it played.
        new CapturesRows(mPage, CapturesRows.of(mMod.id), false)
                .build(getString(R.string.app_captures));

        // What it can do, with the addresses it may reach.
        mPerms = column();
        mPage.group(R.string.mod_sec_can_do).view(mPerms);
        refreshPermissions();

        LinearLayout det = column();
        Map<String, Object> src = ModJson.map(mMod.install.get("source"));
        ModuleUi.dim(det, getString(R.string.mod_details,
                mMod.id,
                String.valueOf(mMod.install.get("installed_at")),
                src != null && src.get("name") != null ? String.valueOf(src.get("name"))
                        : "file",
                String.valueOf(mMod.install.get("tree_sha256")).substring(0, 16),
                fp == null ? "?" : fp)).setTextIsSelectable(true);
        mPage.group(R.string.mod_sec_details).view(det);
        actions();
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (mPage != null) {
            mPage.refresh();
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (mPage != null) {
            mPage.onActivityResult(requestCode, resultCode, data);
        }
    }

    private LinearLayout column() {
        LinearLayout l = new LinearLayout(this);
        l.setOrientation(LinearLayout.VERTICAL);
        return l;
    }

    /** Update from file (the same install sheet; it shows the version change) and Remove. */
    private void actions() {
        Page.Group g = mPage.group(0);
        g.link(getString(R.string.app_update), getString(R.string.app_update_sum),
                this::pickUpdate);
        LinearLayout remove = g.link(getString(R.string.mod_remove), null,
                () -> new AlertDialog.Builder(this)
                .setTitle(getString(R.string.mod_remove_title, mMod.name()))
                .setMessage(R.string.mod_remove_body)
                .setPositiveButton(R.string.mod_remove, (d, w) -> remove())
                .setNegativeButton(R.string.cancel, null)
                .show());
        ((android.widget.TextView) ((LinearLayout) remove.getChildAt(0)).getChildAt(0))
                .setTextColor(getColor(R.color.abyss_warn));
    }

    private void pickUpdate() {
        Intent i = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("*/*")
                .putExtra(Intent.EXTRA_MIME_TYPES, new String[] {"application/zip",
                        "application/octet-stream", "application/x-abyss-app"});
        try {
            mPage.startForResult(i, (code, data) -> {
                if (code == RESULT_OK && data != null && data.getData() != null) {
                    // Back from the install sheet: rebuild, the version or settings may differ.
                    mPage.startForResult(new Intent(this, ModuleInstallActivity.class)
                            .setData(data.getData())
                            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
                            (c2, d2) -> recreate());
                }
            });
        } catch (ActivityNotFoundException e) {
            Log.w(TAG, "no document picker", e);
            Toast.makeText(this, R.string.mod_no_picker, Toast.LENGTH_SHORT).show();
        }
    }

    private Set<String> newOrigins(Map<String, Object> vals) {
        Set<String> o = mSpec.netOrigins(vals);
        o.removeAll(mMod.approvedNet());
        return o;
    }

    private void refreshPermissions() {
        Map<String, Object> vals = mForm.values();
        mPerms.removeAllViews();
        for (ModuleSpec.Permission p : mSpec.permissions(vals)) {
            ModuleUi.body(mPerms, "· " + p.words);
        }
        // A new address needs a hold, like the install.
        mApprove.removeAllViews();
        Set<String> fresh = newOrigins(vals);
        mSave.setVisibility(fresh.isEmpty() ? View.VISIBLE : View.GONE);
        if (!fresh.isEmpty()) {
            List<String> l = new ArrayList<>(fresh);
            String o = String.join(", ", l);
            ModuleUi.warn(mApprove, getString(ModuleSpec.isLan(l.get(0))
                    ? R.string.mod_new_address_lan : R.string.mod_new_address_net, o));
            ModuleUi.hold(mApprove, getString(R.string.mod_hold_allow), () -> save(true));
        }
    }

    private void save(boolean approvedNow) {
        if (!mForm.validate()) {
            return;
        }
        Map<String, Object> vals = mForm.values();
        boolean needsApproval = !newOrigins(vals).isEmpty();
        if (needsApproval && !approvedNow) {
            refreshPermissions();
            return;
        }
        try {
            if (needsApproval) {
                mStore.approveNet(mMod, mSpec, vals, System.currentTimeMillis());
            }
            mStore.writeSettings(mSpec, vals);
            Toast.makeText(this, R.string.mod_saved, Toast.LENGTH_LONG).show();
            finish();
        } catch (IOException | RuntimeException e) {
            Log.w(TAG, "save settings of " + mMod.id, e);
            Toast.makeText(this, R.string.mod_save_failed, Toast.LENGTH_LONG).show();
        }
    }

    private void remove() {
        try {
            mStore.uninstall(mMod.id);
            Toast.makeText(this, getString(R.string.mod_removed, mMod.name()),
                    Toast.LENGTH_SHORT).show();
            finish();
        } catch (IOException | RuntimeException e) {
            Log.w(TAG, "remove " + mMod.id, e);
            Toast.makeText(this, R.string.mod_save_failed, Toast.LENGTH_LONG).show();
        }
    }
}
