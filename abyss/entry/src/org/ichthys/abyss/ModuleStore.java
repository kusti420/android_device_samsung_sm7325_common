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

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.StandardOpenOption;
import java.nio.file.attribute.PosixFilePermissions;
import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Date;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.TimeZone;
import java.util.regex.Pattern;

/**
 * The installed modules under /data/misc/abyss (contract §3):
 *
 * <pre>
 * store/&lt;id&gt;/install.json     install + approval record (status "ready")
 * store/&lt;id&gt;/manifest.json    abyss-app.nix as JSON, normalised, with derived caps
 * store/&lt;id&gt;/menu.json        menu.nix as JSON
 * store/&lt;id&gt;/SIGNATURE        as shipped
 * store/&lt;id&gt;/tree/…           every signed file, read-only
 * conf/settings/app-&lt;id&gt;.env   non-secret settings (#abyss-settings v1)
 * </pre>
 *
 * <p>Atomicity: a module is built completely in {@code store/.staging-<id>-<n>/} (every file
 * fsync-ed), the old one is renamed to {@code store/.old-<id>-<n>}, the new one renamed to
 * {@code store/<id>}, the directory fsync-ed, then the old one deleted. abyssd never runs while
 * Android does (abyss.rc), so it never sees the gap; an interrupted swap is repaired by
 * {@link #list()}. Readers skip every name starting with ".". Settings files are written as
 * NAME.tmp, fsync, rename (files.md rule 2).
 *
 * <p>Ownership: this app is uid system; /data/misc/abyss is 02770 system:system, so new files
 * are group system. abyssd (root, group system, no DAC_OVERRIDE) reads them through the group:
 * files 0640, directories 0750 (signed 755 files 0750). Nothing here is group-writable: abyssd
 * never writes the store. Pure Java (java.nio), so host tests run it on a temp directory.
 */
final class ModuleStore {
    static final String ROOT = "/data/misc/abyss";
    private static final Pattern ID = ModuleSpec.ID;

    /** One installed module as listed. */
    static final class Installed {
        final String id;
        final File dir;
        final Map<String, Object> manifest;
        final Map<String, Object> install;

        Installed(String id, File dir, Map<String, Object> manifest,
                Map<String, Object> install) {
            this.id = id;
            this.dir = dir;
            this.manifest = manifest;
            this.install = install;
        }

        String name() {
            String n = ModJson.str(manifest, "name");
            return n == null ? id : n;
        }

        String version() {
            return String.valueOf(manifest.get("version"));
        }

        String fingerprint() {
            return ModJson.str(ModJson.map(install.get("signer")), "fingerprint");
        }

        @SuppressWarnings("unchecked")
        List<String> approvedNet() {
            Map<String, Object> a = ModJson.map(install.get("approved"));
            List<Object> l = a == null ? null : ModJson.list(a.get("net"));
            List<String> out = new ArrayList<>();
            if (l != null) {
                for (Object o : l) {
                    out.add(String.valueOf(o));
                }
            }
            return out;
        }

        /** Re-checks the installed tree (same rules as at install, features not re-checked). */
        ModuleSpec spec() throws ModuleSpec.Invalid {
            File tree = new File(dir, "tree");
            List<String> files = new ArrayList<>();
            listFiles(tree, "", files);
            return ModuleSpec.parse(read(new File(tree, "abyss-app.nix")),
                    new java.util.HashSet<>(files), p -> read(new File(tree, p)), null);
        }
    }

    private final File mRoot;

    ModuleStore(File root) {
        mRoot = root;
    }

    File storeDir() {
        return new File(mRoot, "store");
    }

    File settingsFile(String id) {
        return new File(mRoot, "conf/settings/app-" + id + ".env");
    }

    // ---- Listing --------------------------------------------------------------------------

    /** Installed modules by name; repairs an interrupted install or uninstall first. */
    List<Installed> list() {
        File store = storeDir();
        String[] names = store.list();
        List<Installed> out = new ArrayList<>();
        if (names == null) {
            return out;
        }
        for (String n : names) {
            File f = new File(store, n);
            if (n.startsWith(".staging-") || n.startsWith(".trash-")) {
                deleteTree(f);
            } else if (n.startsWith(".old-")) {
                String id = n.substring(5, Math.max(5, n.lastIndexOf('-')));
                File cur = new File(store, id);
                if (ID.matcher(id).matches() && !cur.exists()) {
                    f.renameTo(cur); // the swap stopped half way: keep the old version
                } else {
                    deleteTree(f);
                }
            }
        }
        names = store.list();
        if (names == null) {
            return out;
        }
        for (String n : names) {
            if (n.startsWith(".") || !ID.matcher(n).matches()) {
                continue;
            }
            Installed i = get(n);
            if (i != null) {
                out.add(i);
            }
        }
        Collections.sort(out, (a, b) -> a.name().compareToIgnoreCase(b.name()));
        return out;
    }

    Installed get(String id) {
        if (!ID.matcher(id).matches()) {
            return null;
        }
        File d = new File(storeDir(), id);
        try {
            Map<String, Object> inst = ModJson.map(ModJson.parse(read(new File(d,
                    "install.json"))));
            Map<String, Object> man = ModJson.map(ModJson.parse(read(new File(d,
                    "manifest.json"))));
            if (inst == null || man == null || !id.equals(ModJson.str(inst, "id"))) {
                return null;
            }
            return new Installed(id, d, man, inst);
        } catch (RuntimeException e) {
            return null; // a broken entry is not listed (and not used by abyssd)
        }
    }

    // ---- Install / uninstall --------------------------------------------------------------

    /**
     * Writes the module and its settings. {@code approvedBy} is how a person approved it
     * ("touch": the hold-to-install button). {@code sourceName} is the picked file's name.
     */
    void install(ModulePackage pkg, ModuleSpec spec, Map<String, Object> values,
            String approvedBy, String sourceName, long nowMs) throws IOException {
        File store = storeDir();
        mkdirs(store);
        String nonce = Long.toString(nowMs, 36);
        File staging = new File(store, ".staging-" + spec.id + "-" + nonce);
        deleteTree(staging);
        mkdir(staging);
        try {
            File tree = new File(staging, "tree");
            mkdir(tree);
            for (ModulePackage.Entry e : pkg.files.values()) {
                File f = new File(tree, e.path);
                mkdirs(f.getParentFile());
                writeFile(f, e.data, e.executable ? "rwxr-x---" : "rw-r-----");
            }
            writeFile(new File(staging, "SIGNATURE"), pkg.signatureFile, "rw-r-----");
            writeFile(new File(staging, "manifest.json"), utf8(ModJson.write(spec.manifest)),
                    "rw-r-----");
            writeFile(new File(staging, "menu.json"), utf8(ModJson.write(spec.menu)),
                    "rw-r-----");
            writeFile(new File(staging, "install.json"),
                    utf8(ModJson.write(record(pkg, spec, values, approvedBy, sourceName,
                            nowMs))), "rw-r-----");
            fsyncTree(staging);
            File cur = new File(store, spec.id);
            File old = new File(store, ".old-" + spec.id + "-" + nonce);
            if (cur.exists() && !cur.renameTo(old)) {
                throw new IOException("can't move the installed version aside");
            }
            if (!staging.renameTo(cur)) {
                if (old.exists()) {
                    old.renameTo(cur);
                }
                throw new IOException("can't move the new version into place");
            }
            fsyncDir(store);
            deleteTree(old);
        } finally {
            deleteTree(staging);
        }
        writeSettings(spec, values);
    }

    /** Removes the module and its settings file. Secrets (once there are any) are abyssd's. */
    void uninstall(String id) throws IOException {
        if (!ID.matcher(id).matches()) {
            throw new IOException("bad id");
        }
        File cur = new File(storeDir(), id);
        File trash = new File(storeDir(), ".trash-" + id + "-"
                + Long.toString(System.currentTimeMillis(), 36));
        if (cur.exists() && !cur.renameTo(trash)) {
            throw new IOException("can't remove " + id);
        }
        fsyncDir(storeDir());
        deleteTree(trash);
        File s = settingsFile(id);
        if (s.exists() && !s.delete()) {
            throw new IOException("can't remove the settings of " + id);
        }
    }

    /** Records a new approved network address list (the person held the button again). */
    void approveNet(Installed inst, ModuleSpec spec, Map<String, Object> values, long nowMs)
            throws IOException {
        Map<String, Object> rec = new LinkedHashMap<>(inst.install);
        rec.put("approved", approval(spec, values, "touch", nowMs));
        File f = new File(inst.dir, "install.json");
        writeAtomic(f, utf8(ModJson.write(rec)));
    }

    private static Map<String, Object> record(ModulePackage pkg, ModuleSpec spec,
            Map<String, Object> values, String approvedBy, String sourceName, long nowMs) {
        Map<String, Object> r = new LinkedHashMap<>();
        r.put("v", 1L);
        r.put("id", spec.id);
        r.put("name", spec.name);
        r.put("version", spec.version);
        r.put("runtime", spec.runtime);
        r.put("status", "ready");
        Map<String, Object> src = new LinkedHashMap<>();
        src.put("kind", "file");
        if (sourceName != null) {
            src.put("name", sourceName);
        }
        r.put("source", src);
        r.put("tree_sha256", pkg.treeSha256);
        Map<String, Object> signer = new LinkedHashMap<>();
        signer.put("key_type", pkg.signer.keyType);
        signer.put("fingerprint", pkg.signer.fingerprint);
        signer.put("public_key", pkg.signer.publicKey);
        r.put("signer", signer);
        r.put("installed_at", iso(nowMs));
        r.put("installed_at_ms", nowMs);
        r.put("approved", approval(spec, values, approvedBy, nowMs));
        List<Object> req = ModJson.list(ModJson.map(spec.manifest.get("requires")) == null
                ? null : ModJson.map(spec.manifest.get("requires")).get("features"));
        r.put("requires_features", req == null ? new ArrayList<>() : req);
        return r;
    }

    private static Map<String, Object> approval(ModuleSpec spec, Map<String, Object> values,
            String by, long nowMs) {
        Map<String, Object> a = new LinkedHashMap<>();
        a.put("by", by);
        a.put("at", iso(nowMs));
        a.put("at_ms", nowMs);
        a.put("caps", new ArrayList<Object>(spec.caps(values)));
        a.put("net", new ArrayList<Object>(spec.netOrigins(values)));
        return a;
    }

    // ---- Settings (conf/settings/app-<id>.env) ---------------------------------------------

    /** Writes every non-secret setting that has a value or a default (files.md). */
    void writeSettings(ModuleSpec spec, Map<String, Object> values) throws IOException {
        StringBuilder sb = new StringBuilder("#abyss-settings v1\n");
        sb.append("# owner app-").append(spec.id).append(", schema sha256 ")
                .append(ModulePackage.sha256(utf8(ModJson.write(
                        spec.manifest.get("settings") == null ? new LinkedHashMap<>()
                                : spec.manifest.get("settings")))).substring(0, 16))
                .append('\n');
        sb.append("# written by Settings › Abyss › Modules; change it there\n");
        for (ModuleSpec.Setting s : spec.settings) {
            if (s.isSecret()) {
                continue;
            }
            Object v = values == null ? null : values.get(s.key);
            if (v == null) {
                v = s.def;
            }
            if (v == null) {
                continue;
            }
            sb.append(s.env).append('=').append(envQuote(s.envValue(v))).append('\n');
        }
        File f = settingsFile(spec.id);
        mkdirs(f.getParentFile());
        writeAtomic(f, utf8(sb.toString()));
    }

    /** The stored values by setting key (typed per the schema); missing file = empty. */
    Map<String, Object> readSettings(ModuleSpec spec) {
        Map<String, Object> out = new LinkedHashMap<>();
        File f = settingsFile(spec.id);
        if (!f.isFile()) {
            return out;
        }
        Map<String, String> env = new LinkedHashMap<>();
        for (String line : read(f).split("\n")) {
            if (line.isEmpty() || line.startsWith("#") || line.indexOf('=') < 1) {
                continue;
            }
            env.put(line.substring(0, line.indexOf('=')), envUnquote(
                    line.substring(line.indexOf('=') + 1)));
        }
        for (ModuleSpec.Setting s : spec.settings) {
            String raw = env.get(s.env);
            if (raw == null || s.isSecret()) {
                continue;
            }
            Object v;
            switch (s.type) {
                case "bool":
                    v = "true".equals(raw);
                    break;
                case "number":
                    v = s.parse(raw);
                    break;
                default:
                    v = raw;
            }
            if (s.check(v) == null) {
                out.put(s.key, v);
            }
        }
        return out;
    }

    static String envQuote(String v) {
        if (v.matches("[A-Za-z0-9._:/@%+,=-]*")) {
            return v;
        }
        return "\"" + v.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n")
                .replace("$", "\\$") + "\"";
    }

    static String envUnquote(String v) {
        if (v.length() < 2 || !v.startsWith("\"") || !v.endsWith("\"")) {
            return v;
        }
        StringBuilder sb = new StringBuilder();
        String in = v.substring(1, v.length() - 1);
        for (int i = 0; i < in.length(); i++) {
            char c = in.charAt(i);
            if (c == '\\' && i + 1 < in.length()) {
                char n = in.charAt(++i);
                sb.append(n == 'n' ? '\n' : n);
            } else {
                sb.append(c);
            }
        }
        return sb.toString();
    }

    // ---- File helpers ---------------------------------------------------------------------

    static String iso(long ms) {
        SimpleDateFormat f = new SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss'Z'", Locale.ROOT);
        f.setTimeZone(TimeZone.getTimeZone("UTC"));
        return f.format(new Date(ms));
    }

    private static byte[] utf8(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static String read(File f) {
        try {
            return new String(Files.readAllBytes(f.toPath()), StandardCharsets.UTF_8);
        } catch (IOException e) {
            throw new IllegalStateException(f + ": " + e.getMessage(), e);
        }
    }

    private static void listFiles(File dir, String prefix, List<String> out) {
        String[] names = dir.list();
        if (names == null) {
            return;
        }
        for (String n : names) {
            File f = new File(dir, n);
            if (f.isDirectory()) {
                listFiles(f, prefix + n + "/", out);
            } else {
                out.add(prefix + n);
            }
        }
    }

    private static void mkdir(File d) throws IOException {
        if (!d.mkdir() && !d.isDirectory()) {
            throw new IOException("can't create " + d);
        }
        chmod(d, "rwxr-x---");
    }

    private static void mkdirs(File d) throws IOException {
        if (d.isDirectory()) {
            return;
        }
        mkdirs(d.getParentFile());
        mkdir(d);
    }

    private static void chmod(File f, String perms) {
        try {
            Files.setPosixFilePermissions(f.toPath(), PosixFilePermissions.fromString(perms));
        } catch (IOException | UnsupportedOperationException e) {
            // best effort; the directory's group (system) still applies
        }
    }

    private static void writeFile(File f, byte[] data, String perms) throws IOException {
        try (FileOutputStream o = new FileOutputStream(f)) {
            o.write(data);
            o.getFD().sync();
        }
        chmod(f, perms);
    }

    /** NAME.tmp, fsync, rename, fsync the directory. */
    static void writeAtomic(File f, byte[] data) throws IOException {
        File tmp = new File(f.getParentFile(), f.getName() + ".tmp");
        writeFile(tmp, data, "rw-r-----");
        if (!tmp.renameTo(f)) {
            tmp.delete();
            throw new IOException("can't replace " + f);
        }
        fsyncDir(f.getParentFile());
    }

    private static void fsyncDir(File d) {
        try (FileChannel c = FileChannel.open(d.toPath(), StandardOpenOption.READ)) {
            c.force(true);
        } catch (IOException | RuntimeException e) {
            // some platforms can't open a directory for fsync; the rename is still atomic
        }
    }

    private static void fsyncTree(File d) {
        File[] kids = d.listFiles();
        if (kids != null) {
            for (File k : kids) {
                if (k.isDirectory()) {
                    fsyncTree(k);
                }
            }
        }
        fsyncDir(d);
    }

    static void deleteTree(File f) {
        if (f == null || !f.exists() && !Files.isSymbolicLink(f.toPath())) {
            return;
        }
        if (f.isDirectory() && !Files.isSymbolicLink(f.toPath())) {
            File[] kids = f.listFiles();
            if (kids != null) {
                for (File k : kids) {
                    deleteTree(k);
                }
            }
        }
        f.delete();
    }
}
