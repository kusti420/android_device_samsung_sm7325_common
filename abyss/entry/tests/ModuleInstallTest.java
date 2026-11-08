/*
 * Copyright (C) 2026 IchthysOS
 * Licensed under the Apache License, Version 2.0.
 */
package org.ichthys.abyss;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Function;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

/**
 * Host test of the module installer core (no Android): ModulePackage, SshSig, ModuleSpec,
 * ModuleStore, ModJson. Run with tests/run.sh ARTIFACT.abyssapp.
 */
public final class ModuleInstallTest {
    static int failures;

    static void check(boolean ok, String what) {
        System.out.println((ok ? "ok   " : "FAIL ") + what);
        if (!ok) {
            failures++;
        }
    }

    static byte[] rezip(byte[] zip, Function<Map<String, byte[]>, Map<String, byte[]>> f)
            throws Exception {
        Map<String, byte[]> m = new LinkedHashMap<>();
        try (ZipInputStream z = new ZipInputStream(new ByteArrayInputStream(zip))) {
            ZipEntry e;
            while ((e = z.getNextEntry()) != null) {
                m.put(e.getName(), z.readAllBytes());
            }
        }
        m = f.apply(m);
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        try (ZipOutputStream z = new ZipOutputStream(o)) {
            for (Map.Entry<String, byte[]> e : m.entrySet()) {
                z.putNextEntry(new ZipEntry(e.getKey()));
                z.write(e.getValue());
                z.closeEntry();
            }
        }
        return o.toByteArray();
    }

    static String refusal(byte[] zip) {
        try {
            ModulePackage.read(new ByteArrayInputStream(zip));
            return null;
        } catch (ModulePackage.Refused e) {
            return e.getMessage();
        } catch (Exception e) {
            return "other: " + e;
        }
    }

    static String invalid(byte[] zip) throws Exception {
        try {
            ModuleSpec.parse(ModulePackage.read(new ByteArrayInputStream(zip)),
                    ModuleSpec.servedFeatures());
            return null;
        } catch (ModuleSpec.Invalid e) {
            return e.getMessage();
        }
    }

    public static void main(String[] a) throws Exception {
        byte[] zip = Files.readAllBytes(new File(a[0]).toPath());
        String expectFp = a.length > 1 ? a[1] : null;

        ModulePackage pkg = ModulePackage.read(new ByteArrayInputStream(zip));
        check(pkg.files.containsKey("abyss-app.nix") && pkg.files.containsKey("menu.nix"),
                "artifact reads, " + pkg.files.size() + " signed files");
        check(expectFp == null || expectFp.equals(pkg.signer.fingerprint),
                "signer " + pkg.signer.fingerprint + " (" + pkg.signer.keyType + ")");

        ModuleSpec spec = ModuleSpec.parse(pkg, ModuleSpec.servedFeatures());
        check("gabbee-radio".equals(spec.id) && "pure".equals(spec.runtime)
                && "music".equals(spec.category), "manifest: id/runtime/category");
        check(spec.settings.size() == 3 && "SERVER".equals(spec.setting("server").env)
                && "PREBUFFER_SECONDS".equals(spec.setting("prebufferSeconds").env),
                "settings + env names");
        check(spec.streams && spec.loads && spec.captures, "derived: streams, loads, captures");
        List<String> caps = spec.caps(new LinkedHashMap<>());
        System.out.println("     caps " + caps);
        // The artifact's own server default (a LAN address in the radio module), not a fixed one.
        String srvDefault = String.valueOf(spec.setting("server").def);
        String srvOrigin = spec.netOrigins(new LinkedHashMap<>()).iterator().next();
        check(caps.contains("net " + srvOrigin + (ModuleSpec.isLan(srvOrigin) ? " lan" : "")
                        + " (links from it: same origin only)"),
                "net cap from the server setting default");
        for (ModuleSpec.Permission p : spec.permissions(new LinkedHashMap<>())) {
            System.out.println("     · " + p.words);
        }
        Map<String, Object> pub = new LinkedHashMap<>();
        pub.put("server", "https://radio.example.org");
        check(!ModuleSpec.isLan(spec.netOrigins(pub).iterator().next()),
                "a public server is not LAN");
        check(spec.setting("server").check("http://x/y") != null
                && spec.setting("server").check("http://10.0.0.2:8080") == null,
                "server pattern");
        check(spec.setting("prebufferSeconds").check(40L) != null, "prebuffer max");

        // Tampering
        String r = refusal(rezip(zip, m -> {
            byte[] b = m.get("menu.nix").clone();
            b[b.length - 2] ^= 1;
            m.put("menu.nix", b);
            return m;
        }));
        check(r != null && r.contains("changed after it was signed"), "tampered file: " + r);
        r = refusal(rezip(zip, m -> {
            m.put("extra.txt", "hi".getBytes());
            return m;
        }));
        check(r != null && r.contains("not signed"), "extra file: " + r);
        r = refusal(rezip(zip, m -> {
            m.remove("SIGNATURE");
            return m;
        }));
        check(r != null && r.contains("no SIGNATURE"), "unsigned: " + r);
        r = refusal(rezip(zip, m -> {
            String s = new String(m.get("SIGNATURE"), StandardCharsets.UTF_8);
            int i = s.indexOf("-----END SSH SIGNATURE-----") - 12;
            char c = s.charAt(i);
            m.put("SIGNATURE", (s.substring(0, i) + (c == 'A' ? 'B' : 'A') + s.substring(i + 1))
                    .getBytes(StandardCharsets.UTF_8));
            return m;
        }));
        check(r != null && r.startsWith("Signature not valid"), "forged signature: " + r);
        r = refusal(rezip(zip, m -> {
            m.put("../evil", "x".getBytes());
            return m;
        }));
        check(r != null && r.contains("not allowed"), "path traversal: " + r);

        // Validation errors come with file:line:col (the source parsed straight, unsigned).
        String man = pkg.text("abyss-app.nix");
        java.util.Set<String> files = pkg.files.keySet();
        String bad = parseErr(man.replace("category = \"music\";", "category = \"radio\";"),
                files, pkg);
        check(bad != null && bad.matches("abyss-app\\.nix:\\d+:\\d+: unknown category.*"),
                "bad category: " + bad);
        bad = parseErr(man.replace("runtime = \"pure\";", "runtime = \"pure\"; colour = 1;"),
                files, pkg);
        check(bad != null && bad.contains("unknown key 'colour'"), "unknown key: " + bad);
        bad = parseErr(man.replace("\"capture:fields\" ];", "];"), files, pkg);
        check(bad != null && bad.contains("capture:fields"), "undeclared feature: " + bad);
        String menu = pkg.text("menu.nix");
        bad = parseMenuErr(man, menu.replace("goto = \"tracklist\";", "goto = \"tracklst\";"),
                pkg);
        check(bad != null && bad.matches("menu\\.nix:\\d+:\\d+: screen 'tracklst'.*"),
                "bad goto: " + bad);
        bad = parseMenuErr(man, menu.replace("$settings.format", "$settings.formt"), pkg);
        check(bad != null && bad.contains("no setting 'formt'"), "bad hole: " + bad);

        // Install into a temp root, list, settings, uninstall.
        File root = Files.createTempDirectory("abyss-store").toFile();
        ModuleStore st = new ModuleStore(root);
        Map<String, Object> vals = new LinkedHashMap<>();
        vals.put("format", "mp3");
        st.install(pkg, spec, vals, "touch", "gabbee-radio-0.1.0.abyssapp", 1794100000000L);
        List<ModuleStore.Installed> l = st.list();
        check(l.size() == 1 && "gabb.ee radio".equals(l.get(0).name())
                && "0.1.0".equals(l.get(0).version()), "installed + listed");
        check(new File(root, "store/gabbee-radio/tree/menu.nix").isFile()
                && new File(root, "store/gabbee-radio/menu.json").isFile(), "store layout");
        check(l.get(0).approvedNet().contains(srvOrigin), "approved net");
        String env = ModuleStore.read(st.settingsFile("gabbee-radio"));
        System.out.print(env.replaceAll("(?m)^", "     "));
        check(env.startsWith("#abyss-settings v1\n") && env.contains("\nFORMAT=mp3\n")
                && env.contains("\nSERVER=" + srvDefault + "\n")
                && env.contains("\nPREBUFFER_SECONDS=15\n"), "settings env");
        ModuleSpec again = l.get(0).spec();
        Map<String, Object> back = st.readSettings(again);
        check("mp3".equals(back.get("format")) && Long.valueOf(15).equals(
                back.get("prebufferSeconds")), "settings read back typed: " + back);
        Object mj = ModJson.parse(ModuleStore.read(new File(root,
                "store/gabbee-radio/manifest.json")));
        check(ModJson.list(ModJson.map(mj).get("caps")) != null, "manifest.json has caps");
        // Re-install (update path) and an interrupted swap.
        st.install(pkg, spec, back, "touch", null, 1794100100000L);
        check(st.list().size() == 1, "reinstall keeps one copy");
        new File(root, "store/gabbee-radio").renameTo(new File(root,
                "store/.old-gabbee-radio-abc"));
        check(st.list().size() == 1 && new File(root, "store/gabbee-radio").isDirectory(),
                "interrupted swap repaired");
        st.uninstall("gabbee-radio");
        check(st.list().isEmpty() && !st.settingsFile("gabbee-radio").exists(), "uninstalled");
        ModuleStore.deleteTree(root);

        System.out.println(failures == 0 ? "ALL OK" : failures + " FAILED");
        System.exit(failures == 0 ? 0 : 1);
    }

    static String parseErr(String manifest, java.util.Set<String> files, ModulePackage pkg) {
        try {
            ModuleSpec.parse(manifest, files, pkg::text, ModuleSpec.servedFeatures());
            return null;
        } catch (ModuleSpec.Invalid e) {
            return e.getMessage();
        }
    }

    static String parseMenuErr(String manifest, String menu, ModulePackage pkg) {
        try {
            ModuleSpec.parse(manifest, pkg.files.keySet(),
                    p -> p.equals("menu.nix") ? menu : pkg.text(p), ModuleSpec.servedFeatures());
            return null;
        } catch (ModuleSpec.Invalid e) {
            return e.getMessage();
        }
    }
}
