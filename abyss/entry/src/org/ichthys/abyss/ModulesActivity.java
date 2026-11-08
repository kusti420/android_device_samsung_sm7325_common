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
import android.net.Uri;
import android.os.Bundle;
import android.util.Log;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.Toast;

import java.io.File;
import java.util.List;
import java.util.Map;

/**
 * Settings › Abyss › Modules: what is installed (from /data/misc/abyss/store), "Install from
 * file…" (Storage Access Framework picker for a .abyssapp), "Add source…" (git repos: coming
 * with Ichthys Apps fetching; ichthys.nix has no abyss.* options yet). Tapping a module opens
 * its settings page. Everything that needs typing (server addresses, keys) is set here in
 * Android, never inside Abyss (owner decision 1.5).
 */
public final class ModulesActivity extends Activity {
    private static final String TAG = "AbyssModules";
    private static final int REQ_PICK = 1;
    private static final int REQ_INSTALL = 2;

    private LinearLayout mList;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        LinearLayout col = ModuleUi.page(this, false);
        ModuleUi.title(col, getString(R.string.mod_title));
        ModuleUi.dim(col, getString(R.string.mod_intro));

        ModuleUi.section(col, getString(R.string.mod_sec_installed));
        LinearLayout card = ModuleUi.card(col);
        mList = new LinearLayout(this);
        mList.setOrientation(LinearLayout.VERTICAL);
        card.addView(mList, ModuleUi.lp(this, 0));

        ModuleUi.section(col, getString(R.string.mod_sec_add));
        LinearLayout add = ModuleUi.card(col);
        ModuleUi.button(add, getString(R.string.mod_install_file), v -> pick(), 0);
        ModuleUi.dim(add, getString(R.string.mod_install_file_hint));
        ModuleUi.button(add, getString(R.string.mod_add_source), v -> addSource(), 16);
    }

    @Override
    protected void onResume() {
        super.onResume();
        refresh();
    }

    private void refresh() {
        mList.removeAllViews();
        ModuleStore store = new ModuleStore(new File(ModuleStore.ROOT));
        List<ModuleStore.Installed> mods = store.list();
        if (mods.isEmpty()) {
            ModuleUi.dim(mList, getString(R.string.mod_none)).setPadding(0, 0, 0, 0);
            return;
        }
        for (ModuleStore.Installed m : mods) {
            String fp = m.fingerprint();
            String sub = getString(R.string.mod_row_sub, m.version(),
                    String.valueOf(m.manifest.get("category")),
                    fp == null ? "?" : fp.substring(0, Math.min(fp.length(), 15)) + "…");
            String setup = needsSetup(store, m);
            if (setup != null) {
                sub = getString(R.string.mod_needs_setup) + " · " + sub;
            }
            LinearLayout row = ModuleUi.row(mList, m.name(), sub, v -> startActivity(
                    new Intent(this, ModuleSettingsActivity.class)
                            .putExtra(ModuleSettingsActivity.EXTRA_ID, m.id)));
            if (setup != null) {
                ((TextView) row.getChildAt(1)).setTextColor(getColor(R.color.abyss_warn));
            }
        }
    }

    /** The first required setting without a value, or null. */
    static String needsSetup(ModuleStore store, ModuleStore.Installed m) {
        try {
            ModuleSpec spec = m.spec();
            Map<String, Object> vals = store.readSettings(spec);
            for (ModuleSpec.Setting s : spec.settings) {
                if (!s.isSecret() && s.check(vals.containsKey(s.key) ? vals.get(s.key)
                        : s.def) != null) {
                    return s.label;
                }
            }
            return null;
        } catch (ModuleSpec.Invalid | RuntimeException e) {
            return e.getMessage();
        }
    }

    private void pick() {
        Intent i = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("*/*")
                .putExtra(Intent.EXTRA_MIME_TYPES, new String[] {"application/zip",
                        "application/octet-stream", "application/x-abyss-app"});
        try {
            startActivityForResult(i, REQ_PICK);
        } catch (ActivityNotFoundException e) {
            Log.w(TAG, "no document picker", e);
            Toast.makeText(this, R.string.mod_no_picker, Toast.LENGTH_SHORT).show();
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQ_PICK && resultCode == RESULT_OK && data != null
                && data.getData() != null) {
            Uri uri = data.getData();
            startActivityForResult(new Intent(this, ModuleInstallActivity.class)
                    .setData(uri)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION), REQ_INSTALL);
        }
    }

    /**
     * Git sources need abyss.sources in ichthys.nix and Ichthys Apps fetching (v1.2); neither
     * exists yet, so this says so instead of pretending to record anything.
     */
    private void addSource() {
        new AlertDialog.Builder(this)
                .setTitle(R.string.mod_add_source)
                .setMessage(R.string.mod_add_source_soon)
                .setPositiveButton(android.R.string.ok, null)
                .show();
    }
}
