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
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.net.Uri;
import android.text.TextUtils;
import android.text.format.DateUtils;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.Toast;

import java.io.IOException;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

/**
 * The Captures section: what abyssd saved on Volume Down + Power ({@link Captures}). Tap a
 * line to copy it, touch and hold to share or delete it; Share all, Save to file (Storage
 * Access Framework) and Clear all below the list.
 */
final class CapturesRows {
    private static final String TAG = "AbyssEntry";

    private final Page mPage;
    private final Activity mA;
    private TextView mInfo;
    private LinearLayout mList;
    private View mActions;
    private List<Captures.Item> mCaptures = new ArrayList<>();

    CapturesRows(Page p) {
        mPage = p;
        mA = p.a;
    }

    static CharSequence summary(Context c) {
        Captures.Loaded l = Captures.load();
        if (l.error) {
            return c.getString(R.string.captures_unreadable);
        }
        return l.items.isEmpty() ? c.getString(R.string.captures_none_short)
                : c.getString(R.string.captures_count, l.items.size());
    }

    void build() {
        Page.Group g = mPage.group(0);
        LinearLayout col = new LinearLayout(mA);
        col.setOrientation(LinearLayout.VERTICAL);
        mInfo = mPage.summary(null);
        col.addView(mInfo);
        mList = new LinearLayout(mA);
        mList.setOrientation(LinearLayout.VERTICAL);
        col.addView(mList, mPage.lp(8));
        g.view(col);

        LinearLayout actions = new LinearLayout(mA);
        actions.setOrientation(LinearLayout.HORIZONTAL);
        actions.addView(action(R.string.captures_share_all, R.color.abyss_text, 0,
                v -> shareText(Captures.asText(mCaptures))));
        actions.addView(action(R.string.captures_save_file, R.color.abyss_text, 8,
                v -> saveToFile()));
        actions.addView(action(R.string.captures_clear_all, R.color.abyss_warn, 8,
                v -> confirmClearAll()));
        mActions = actions;
        mPage.addToPage(actions, 16);
        mPage.onRefresh(this::refresh);
    }

    private TextView action(int label, int color, int startDp, View.OnClickListener l) {
        TextView b = mPage.text(mA.getString(label), 15, color, mPage.bodyFace(true));
        b.setGravity(Gravity.CENTER);
        b.setMinHeight(mPage.dp(48));
        b.setPadding(mPage.dp(6), 0, mPage.dp(6), 0);
        b.setBackground(mPage.outlineDrawable());
        b.setClickable(true);
        b.setFocusable(true);
        b.setOnClickListener(l);
        LinearLayout.LayoutParams p = new LinearLayout.LayoutParams(0,
                ViewGroup.LayoutParams.WRAP_CONTENT, 1f);
        p.setMarginStart(mPage.dp(startDp));
        b.setLayoutParams(p);
        return b;
    }

    private void refresh() {
        Captures.Loaded loaded = Captures.load();
        mCaptures = loaded.items;
        mList.removeAllViews();
        int n = mCaptures.size();
        if (loaded.error) {
            mInfo.setText(R.string.captures_unreadable);
            mInfo.setTextColor(mA.getColor(R.color.abyss_warn));
        } else {
            mInfo.setText(n == 0 ? R.string.captures_empty : R.string.captures_hint);
            mInfo.setTextColor(mA.getColor(n == 0 ? R.color.abyss_text : R.color.abyss_dim));
        }
        mList.setVisibility(n == 0 ? View.GONE : View.VISIBLE);
        mActions.setVisibility(n == 0 ? View.GONE : View.VISIBLE);
        for (Captures.Item it : mCaptures) {
            mList.addView(row(it));
        }
    }

    private View row(Captures.Item it) {
        LinearLayout row = new LinearLayout(mA);
        row.setOrientation(LinearLayout.VERTICAL);
        android.util.TypedValue tv = new android.util.TypedValue();
        mA.getTheme().resolveAttribute(android.R.attr.selectableItemBackground, tv, true);
        row.setBackgroundResource(tv.resourceId);
        row.setPadding(0, mPage.dp(10), 0, mPage.dp(10));
        row.setClickable(true);
        row.setFocusable(true);
        row.addView(mPage.text(it.line, 16, R.color.abyss_text, mPage.bodyFace(false)));
        String meta = metaOf(it);
        if (!meta.isEmpty()) {
            TextView sub = mPage.text(meta, 13, R.color.abyss_dim, mPage.bodyFace(false));
            sub.setPadding(0, mPage.dp(2), 0, 0);
            row.addView(sub);
        }
        row.setOnClickListener(v -> copy(it.line));
        row.setOnLongClickListener(v -> {
            actions(it);
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
            parts.add(DateUtils.formatDateTime(mA, it.tsMs, DateUtils.FORMAT_SHOW_DATE
                    | DateUtils.FORMAT_SHOW_TIME | DateUtils.FORMAT_ABBREV_MONTH));
        }
        return TextUtils.join(" · ", parts);
    }

    private void actions(Captures.Item it) {
        CharSequence[] items = {
                mA.getString(R.string.capture_copy),
                mA.getString(R.string.capture_share),
                mA.getString(R.string.capture_delete),
        };
        new AlertDialog.Builder(mA)
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
        ClipboardManager cm = mA.getSystemService(ClipboardManager.class);
        cm.setPrimaryClip(ClipData.newPlainText(mA.getString(R.string.captures_share_subject),
                text));
        // Android 13+ shows its own "copied" confirmation; no toast on top of it.
    }

    private void shareText(String text) {
        Intent send = new Intent(Intent.ACTION_SEND)
                .setType("text/plain")
                .putExtra(Intent.EXTRA_TEXT, text)
                .putExtra(Intent.EXTRA_SUBJECT, mA.getString(R.string.captures_share_subject));
        try {
            mA.startActivity(Intent.createChooser(send, null));
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
        rewrite(keep, R.string.captures_deleted);
    }

    private void confirmClearAll() {
        new AlertDialog.Builder(mA)
                .setTitle(R.string.captures_clear_title)
                .setMessage(mA.getString(R.string.captures_clear_body, mCaptures.size()))
                .setPositiveButton(R.string.captures_clear_ok,
                        (d, w) -> rewrite(new ArrayList<>(), 0))
                .setNegativeButton(R.string.cancel, null)
                .show();
    }

    private void rewrite(List<Captures.Item> keep, int doneRes) {
        // Re-read first: the list on screen is what we change, but never drop lines that are
        // in the file and not on screen (there are none while Android runs; belt and braces).
        Captures.Loaded now = Captures.load();
        if (now.error || now.items.size() != mCaptures.size()) {
            refresh();
            Toast.makeText(mA, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
            return;
        }
        if (Captures.rewrite(keep)) {
            if (doneRes != 0) {
                Toast.makeText(mA, doneRes, Toast.LENGTH_SHORT).show();
            }
        } else {
            Toast.makeText(mA, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
        }
        refresh();
    }

    /** Storage Access Framework: the user picks where the text file goes. */
    private void saveToFile() {
        Intent i = new Intent(Intent.ACTION_CREATE_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("text/plain")
                .putExtra(Intent.EXTRA_TITLE, mA.getString(R.string.captures_file_name));
        try {
            mPage.startForResult(i, this::onSaved);
        } catch (ActivityNotFoundException e) {
            Log.w(TAG, "no document picker", e);
            Toast.makeText(mA, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
        }
    }

    private void onSaved(int resultCode, Intent data) {
        if (resultCode != Activity.RESULT_OK || data == null || data.getData() == null) {
            return;
        }
        Uri uri = data.getData();
        List<Captures.Item> items = Captures.load().items;
        try (OutputStream out = mA.getContentResolver().openOutputStream(uri, "wt")) {
            if (out == null) {
                throw new IOException("no stream for " + uri);
            }
            out.write(Captures.asText(items).getBytes(StandardCharsets.UTF_8));
            Toast.makeText(mA, mA.getString(R.string.captures_saved_to_file, items.size()),
                    Toast.LENGTH_SHORT).show();
        } catch (IOException | RuntimeException e) {
            Log.w(TAG, "save captures to " + uri, e);
            Toast.makeText(mA, R.string.captures_write_failed, Toast.LENGTH_SHORT).show();
        }
    }
}
