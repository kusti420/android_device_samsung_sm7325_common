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
import java.util.function.Predicate;

/**
 * The captures of one Abyss app, on that app's page: what abyssd saved on Volume Down + Power
 * while it played ({@link Captures}, attributed by {@link Captures#owner}). Tap a line to copy
 * it, touch and hold to share or delete it; Share all, Save to file (Storage Access Framework)
 * and Clear all (this app's only) below the list. Deleting rewrites the whole file and keeps
 * every other app's lines.
 */
final class CapturesRows {
    private static final String TAG = "AbyssEntry";

    private final Page mPage;
    private final Activity mA;
    private TextView mInfo;
    private LinearLayout mList;
    private View mActions;
    private final Predicate<Captures.Item> mFilter;
    private final boolean mHideWhenEmpty;
    private TextView mHeader;
    private View mCard;
    /** Everything in the file (for rewrites) and the part this page shows. */
    private List<Captures.Item> mAll = new ArrayList<>();
    private List<Captures.Item> mCaptures = new ArrayList<>();

    /**
     * {@code filter}: which captures this page lists. {@code hideWhenEmpty}: the whole group
     * disappears while there are none (Advanced › captures of removed apps).
     */
    CapturesRows(Page p, Predicate<Captures.Item> filter, boolean hideWhenEmpty) {
        mPage = p;
        mA = p.a;
        mFilter = filter;
        mHideWhenEmpty = hideWhenEmpty;
    }

    /** Captures of {@code owner} (a module id or {@link Captures#OWNER_MUSIC}). */
    static Predicate<Captures.Item> of(String owner) {
        return it -> owner.equals(it.owner);
    }

    /** How many captures {@code owner} has: for the app rows on the top page. */
    static int count(String owner) {
        int n = 0;
        for (Captures.Item it : Captures.load().items) {
            if (owner.equals(it.owner)) {
                n++;
            }
        }
        return n;
    }

    private List<Captures.Item> filter(List<Captures.Item> all) {
        List<Captures.Item> out = new ArrayList<>();
        for (Captures.Item it : all) {
            if (mFilter.test(it)) {
                out.add(it);
            }
        }
        return out;
    }

    /** Adds the group, titled {@code title}, to the page. */
    void build(CharSequence title) {
        Page.Group g = mPage.group(title);
        mHeader = mPage.lastGroupTitle();
        mCard = g.card;
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
        mAll = loaded.items;
        mCaptures = filter(mAll);
        mList.removeAllViews();
        int n = mCaptures.size();
        boolean hide = mHideWhenEmpty && n == 0 && !loaded.error;
        mCard.setVisibility(hide ? View.GONE : View.VISIBLE);
        if (mHeader != null) {
            mHeader.setVisibility(hide ? View.GONE : View.VISIBLE);
        }
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
        List<Captures.Item> keep = new ArrayList<>(mAll);
        // Remove exactly this row (identity), not every line with the same text.
        for (int i = 0; i < keep.size(); i++) {
            if (keep.get(i) == it) {
                keep.remove(i);
                break;
            }
        }
        rewrite(keep, R.string.captures_deleted);
    }

    /** Every line of the file except this page's. */
    private List<Captures.Item> allButShown() {
        List<Captures.Item> keep = new ArrayList<>();
        for (Captures.Item it : mAll) {
            if (!mFilter.test(it)) {
                keep.add(it);
            }
        }
        return keep;
    }

    private void confirmClearAll() {
        new AlertDialog.Builder(mA)
                .setTitle(R.string.captures_clear_title)
                .setMessage(mA.getString(R.string.captures_clear_body, mCaptures.size()))
                .setPositiveButton(R.string.captures_clear_ok,
                        (d, w) -> rewrite(allButShown(), 0))
                .setNegativeButton(R.string.cancel, null)
                .show();
    }

    private void rewrite(List<Captures.Item> keep, int doneRes) {
        // Re-read first: the list on screen is what we change, but never drop lines that are
        // in the file and not on screen (there are none while Android runs; belt and braces).
        Captures.Loaded now = Captures.load();
        if (now.error || now.items.size() != mAll.size()) {
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
        List<Captures.Item> items = filter(Captures.load().items);
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
