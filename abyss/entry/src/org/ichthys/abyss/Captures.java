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

import android.system.ErrnoException;
import android.system.Os;
import android.util.Log;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * /data/misc/abyss/out/captures.jsonl: what abyssd saved on Volume Down + Power in Abyss.
 * One JSON object per line: {"kind": "music", "line": "Artist - Title (Mix) (LABEL CATNO)",
 * "fields": {...}, "time_ms": ...} (abyssd capture.rs). Each capture is listed on the page of
 * the Abyss app it was made in ({@link #owner}).
 *
 * <p>Writers. abyssd appends while Abyss is active; this app rewrites the file (delete one,
 * clear all) only while Android runs. The framework, and with it this process, is stopped for
 * the whole time abyssd runs (abyss.rc), so the two never write at the same time. Rewrites are
 * atomic (temporary file in out/, fsync, rename), like abyssd's own state files.
 *
 * <p>Permissions. out/ is 02770 system:system (abyss.rc). abyssd runs as root, group system,
 * WITHOUT CAP_DAC_OVERRIDE, umask 007: its files are root:system 0660, which this app (uid system)
 * reads through the group. A file this app writes is system:system and must stay group-writable
 * (0660), or abyssd could no longer append to it: the app umask is 077, so it is chmod-ed.
 * SELinux: abyss/sepolicy/private/abyss_entry.te.
 */
final class Captures {
    private static final String TAG = "AbyssEntry";
    static final String DIR = "/data/misc/abyss/out";
    static final String FILE = DIR + "/captures.jsonl";
    private static final String TMP = DIR + "/captures.jsonl.entry-tmp";
    /** Guard against a runaway file: the list shows the newest this many. */
    private static final int MAX_SHOWN = 2000;

    /** One saved line. {@link #raw} is kept so rewrites keep abyssd's exact JSON. */
    static final class Item {
        final String raw;
        final String kind;
        final String line;
        /** Milliseconds since the epoch, or 0 when the line has no usable time. */
        final long tsMs;
        /** {@link #owner}: whose page lists it. */
        final String owner;

        Item(String raw, String kind, String line, long tsMs, String owner) {
            this.raw = raw;
            this.kind = kind;
            this.line = line;
            this.tsMs = tsMs;
            this.owner = owner;
        }
    }

    /** {@link #owner} of a capture made while the built-in music player played. */
    static final String OWNER_MUSIC = "music";

    /**
     * Which Abyss app a capture belongs to (each app's page lists its own): fields.module (the
     * module id, if abyssd ever stamps it), else fields.source (both radio modules put their
     * module id there: gabb.ee's server sends source = "gabbee-radio", WeAreOne.FM's manifest
     * capture mapping sets source = "weareone-fm"), else {@link #OWNER_MUSIC} for kind "music"
     * (capture.rs MusicSource, the library player), else "".
     */
    static String owner(String kind, JSONObject fields) {
        if (fields != null) {
            for (String k : new String[] {"module", "source"}) {
                String v = fields.optString(k, "").trim();
                if (!v.isEmpty()) {
                    return v;
                }
            }
        }
        return "music".equals(kind) ? OWNER_MUSIC : "";
    }

    /** Result of {@link #load}: items newest first, or {@code error} when the file is unreadable. */
    static final class Loaded {
        final List<Item> items;
        final boolean error;

        Loaded(List<Item> items, boolean error) {
            this.items = items;
            this.error = error;
        }
    }

    private Captures() {}

    /** Reads the file. A missing file is an empty list, not an error. Never throws. */
    static Loaded load() {
        List<String> lines;
        try {
            lines = Files.readAllLines(new File(FILE).toPath(), StandardCharsets.UTF_8);
        } catch (java.nio.file.NoSuchFileException e) {
            return new Loaded(new ArrayList<>(), false);
        } catch (IOException | SecurityException e) {
            Log.w(TAG, "read " + FILE, e);
            return new Loaded(new ArrayList<>(), true);
        }
        List<Item> items = new ArrayList<>();
        for (String raw : lines) {
            Item it = parse(raw);
            if (it != null) {
                items.add(it);
            }
        }
        // The file is append-only, oldest first; show newest first.
        Collections.reverse(items);
        if (items.size() > MAX_SHOWN) {
            items = new ArrayList<>(items.subList(0, MAX_SHOWN));
        }
        return new Loaded(items, false);
    }

    /** One JSON line, or null if it is blank, broken or has no text to show. */
    static Item parse(String raw) {
        String t = raw.trim();
        if (t.isEmpty()) {
            return null;
        }
        try {
            JSONObject o = new JSONObject(t);
            String line = o.optString("line", "").trim();
            if (line.isEmpty()) {
                return null;
            }
            // abyssd writes time_ms (capture.rs); "ts" is the older name.
            Object ts = o.has("time_ms") ? o.opt("time_ms") : o.opt("ts");
            String kind = o.optString("kind", "");
            return new Item(t, kind, line, tsMillis(ts), owner(kind, o.optJSONObject("fields")));
        } catch (JSONException e) {
            Log.w(TAG, "skipping a broken captures line");
            return null;
        }
    }

    /**
     * abyssd's ts: Unix seconds or milliseconds (number or numeric string). Anything else
     * (e.g. no wall clock yet in Abyss) gives 0 and the row shows no time.
     */
    static long tsMillis(Object ts) {
        double v;
        if (ts instanceof Number) {
            v = ((Number) ts).doubleValue();
        } else if (ts instanceof String) {
            try {
                v = Double.parseDouble(((String) ts).trim());
            } catch (NumberFormatException e) {
                return 0;
            }
        } else {
            return 0;
        }
        if (!(v > 0)) {
            return 0;
        }
        // Below 1e11 it can only be seconds (1e11 s is the year 5138; 1e11 ms is 1973).
        return v < 1e11 ? (long) (v * 1000) : (long) v;
    }

    /**
     * Replaces the file with {@code keep} (newest first, as shown), written back oldest first.
     * An empty list leaves an empty file, so abyssd's next append finds it as before.
     */
    static boolean rewrite(List<Item> keep) {
        StringBuilder sb = new StringBuilder();
        for (int i = keep.size() - 1; i >= 0; i--) {
            sb.append(keep.get(i).raw).append('\n');
        }
        File tmp = new File(TMP);
        try (FileOutputStream out = new FileOutputStream(tmp)) {
            out.write(sb.toString().getBytes(StandardCharsets.UTF_8));
            out.getFD().sync();
        } catch (IOException | SecurityException e) {
            Log.w(TAG, "write " + TMP, e);
            tmp.delete();
            return false;
        }
        try {
            // rw for system (this app) and group system (abyssd: root, group system).
            Os.chmod(TMP, 0660);
            Os.rename(TMP, FILE);
        } catch (ErrnoException e) {
            Log.w(TAG, "replace " + FILE, e);
            tmp.delete();
            return false;
        }
        return true;
    }

    /** Plain text for share / save: one line per capture, newest first. */
    static String asText(List<Item> items) {
        StringBuilder sb = new StringBuilder();
        for (Item it : items) {
            sb.append(it.line).append('\n');
        }
        return sb.toString();
    }
}
