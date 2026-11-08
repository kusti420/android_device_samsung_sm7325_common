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

import com.android.internal.ichthys.config.NixParseException;
import com.android.internal.ichthys.config.NixParser;

import java.io.File;
import java.io.IOException;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.regex.PatternSyntaxException;

/**
 * A checked app module: abyss-app.nix + menu.nix parsed (NixParser, the ichthys.nix parser),
 * validated against app-manifest.schema.json and the menu rules, normalised into the installed
 * JSON form, with its capabilities derived from what it uses.
 *
 * <p>This build installs {@code runtime = "pure"} modules only: script and container modules
 * need the Termux staging and store pipeline (v1.1/v1.2), which isn't here yet. The contract is
 * ~/abyss-design/reports/module-install-contract.md. Pure Java except NixParser, which is itself
 * pure Java (framework, com.android.internal.ichthys.config), so host tests can run it.
 */
final class ModuleSpec {
    /** App API levels this build serves. */
    static final List<Long> API_LEVELS = Collections.singletonList(1L);
    /** Written by the ROM next to abyssd: the features it implements, one per line. */
    static final String FEATURES_FILE = "/system_ext/etc/abyss/features";
    /**
     * Used while {@link #FEATURES_FILE} is absent (bring-up): the features the module host is
     * being built with (contract §6). Remove once abyssd ships the file.
     */
    static final List<String> FALLBACK_FEATURES = Arrays.asList(
            "state", "job", "post", "prompt", "event", "apps", "settings", "post:capture",
            "post:player", "needs:media", "radio:wifi", "menu:get", "media:stream",
            "media:stream:radio-v1", "media:stream:http", "capture:fields");

    static final Set<String> CATEGORIES = new HashSet<>(Arrays.asList(
            "music", "spoken", "time", "home", "info", "tools", "games"));
    private static final Set<String> MANIFEST_KEYS = new HashSet<>(Arrays.asList(
            "id", "name", "version", "api", "requires", "category", "runtime", "about", "authors",
            "license", "icon", "menu", "scripts", "connectors", "secrets", "settings", "actions",
            "suggest", "oci", "bell", "state", "events", "accepts", "tiles", "fetch", "capture",
            "data"));
    private static final Set<String> SETTING_KEYS = new HashSet<>(Arrays.asList(
            "type", "label", "help", "default", "optional", "env", "min", "max", "step",
            "integer", "unit", "minLength", "maxLength", "pattern", "choices", "pick", "test"));
    private static final Set<String> RESERVED_ENV = new HashSet<>(Arrays.asList(
            "PATH", "HOME", "SHELL", "USER", "LOGNAME", "PWD", "OLDPWD", "IFS", "TMPDIR", "TERM",
            "LANG", "PREFIX", "BASH_ENV", "ENV", "CDPATH"));
    static final Pattern ID = Pattern.compile("[a-z0-9][a-z0-9-]{0,31}");
    private static final Pattern SEMVER =
            Pattern.compile("[0-9]+\\.[0-9]+\\.[0-9]+([-+][0-9A-Za-z.-]+)?");
    private static final Pattern SETTING_KEY = Pattern.compile("[a-zA-Z][a-zA-Z0-9]*");
    private static final Pattern ENV = Pattern.compile("[A-Z_][A-Z0-9_]*");
    private static final Pattern FEATURE = Pattern.compile(
            "([a-z0-9][a-z0-9-]*(:[a-z0-9][a-z0-9-]*)*|x-[a-z0-9]{2,16}(:[a-z0-9][a-z0-9-]*)+)");
    /** A hole: $ns(.key)*(:format)?; a key may contain '-' between key characters. */
    static final Pattern HOLE = Pattern.compile(
            "\\$([A-Za-z_][A-Za-z0-9_]*)((?:\\.[A-Za-z0-9_]+(?:-[A-Za-z0-9_]+)*)*)"
                    + "(?::(time|clock|dur)\\b)?");

    // Menu (menu api 1 + the proposed menu:get / media:stream additions, contract §5).
    private static final Set<String> SCREEN_KINDS = new HashSet<>(Arrays.asList(
            "list", "text", "number", "choice", "confirm", "progress"));
    private static final Set<String> SCREEN_KEYS = new HashSet<>(Arrays.asList(
            "kind", "title", "sub", "items", "lines", "min", "max", "step", "value", "run",
            "args", "choices", "text", "get", "empty", "error", "guide"));
    private static final Set<String> ITEM_KEYS = new HashSet<>(Arrays.asList(
            "label", "sub", "run", "goto", "back", "media", "say", "buzz", "show", "timer", "pick",
            "battery", "stream", "args", "reply", "as"));
    private static final List<String> ITEM_ACTIONS = Arrays.asList(
            "run", "goto", "back", "media", "say", "buzz", "show", "timer", "pick", "battery",
            "stream");
    private static final Set<String> STREAM_KEYS = new HashSet<>(Arrays.asList(
            "protocol", "url", "formats", "format", "prebuffer", "songs", "tag", "title",
            "show"));
    private static final Set<String> STREAM_PROTOCOLS = new HashSet<>(Arrays.asList(
            "radio-v1", "http"));
    private static final Set<String> GET_KEYS = new HashSet<>(Arrays.asList(
            "url", "every", "refreshAt", "page"));
    private static final Set<String> RESERVED_NS = new HashSet<>(Arrays.asList(
            "state", "settings", "battery", "time", "value", "data", "row", "stream", "abyss",
            "pub"));
    private static final Pattern DURATION = Pattern.compile("[0-9]+(ms|s|m|h)");

    /** The manifest or menu is not acceptable; message starts with FILE:LINE:COL. */
    static final class Invalid extends Exception {
        private static final long serialVersionUID = 1L;

        Invalid(String msg) {
            super(msg);
        }
    }

    /** One user setting, normalised. */
    static final class Setting {
        String key;
        String type;
        String label;
        String help;
        Object def;
        boolean optional;
        String env;
        Double min;
        Double max;
        double step = 1;
        boolean integer;
        String unit;
        int minLength;
        int maxLength = 200;
        String pattern;
        final List<String[]> choices = new ArrayList<>();

        boolean isSecret() {
            return "secret".equals(type);
        }

        /** null when v is acceptable, else the reason in plain words. */
        String check(Object v) {
            if (v == null) {
                return optional || isSecret() ? null : label + " is needed";
            }
            switch (type) {
                case "text":
                case "secret": {
                    if (!(v instanceof String)) {
                        return label + " must be text";
                    }
                    String s = (String) v;
                    if (s.length() < minLength || s.length() > maxLength) {
                        return label + ": " + minLength + " to " + maxLength + " characters";
                    }
                    if (pattern != null && !Pattern.compile(pattern).matcher(s).matches()) {
                        return label + " doesn't look right";
                    }
                    return null;
                }
                case "number": {
                    if (!(v instanceof Long || v instanceof Double)) {
                        return label + " must be a number";
                    }
                    double d = ((Number) v).doubleValue();
                    if (integer && d != Math.rint(d)) {
                        return label + " must be a whole number";
                    }
                    if (min != null && d < min || max != null && d > max) {
                        return label + " must be " + fmt(min) + " to " + fmt(max)
                                + (unit == null ? "" : " " + unit);
                    }
                    return null;
                }
                case "bool":
                    return v instanceof Boolean ? null : label + " must be on or off";
                case "enum":
                    for (String[] c : choices) {
                        if (c[0].equals(v)) {
                            return null;
                        }
                    }
                    return label + ": not one of the choices";
                default:
                    return "unknown type";
            }
        }

        /** Text typed in a field → the typed value (null = empty). */
        Object parse(String text) {
            String t = text == null ? "" : text.trim();
            if (t.isEmpty()) {
                return null;
            }
            if ("number".equals(type)) {
                try {
                    return integer || t.matches("-?[0-9]+") ? (Object) Long.parseLong(t)
                            : (Object) Double.parseDouble(t);
                } catch (NumberFormatException e) {
                    return t; // check() then says "must be a number"
                }
            }
            return "secret".equals(type) ? text : t;
        }

        /** The dotenv form of a value (bool true|false). */
        String envValue(Object v) {
            if (v instanceof Double && ((Double) v) == Math.rint((Double) v)) {
                return Long.toString(((Double) v).longValue());
            }
            return String.valueOf(v);
        }
    }

    /** One permission line on the install sheet: plain words, plus the technical cap. */
    static final class Permission {
        final String words;
        final String cap;

        Permission(String words, String cap) {
            this.words = words;
            this.cap = cap;
        }
    }

    final String id;
    final String name;
    final String version;
    final String about;
    final String category;
    final String runtime;
    final List<String> authors;
    final Map<String, Object> manifest;
    final Map<String, Object> menu;
    final List<Setting> settings;
    /** Setting keys whose value is the start of a network address (contract §4). */
    final Set<String> netSettings;
    /** Fixed origins written literally in the menu. */
    final Set<String> netLiteral;
    final boolean streams;
    final boolean loads;
    final boolean captures;

    private ModuleSpec(Map<String, Object> manifest, Map<String, Object> menu,
            List<Setting> settings, Set<String> netSettings, Set<String> netLiteral,
            boolean streams, boolean loads, boolean captures) {
        this.manifest = manifest;
        this.menu = menu;
        this.settings = settings;
        this.netSettings = netSettings;
        this.netLiteral = netLiteral;
        this.streams = streams;
        this.loads = loads;
        this.captures = captures;
        id = ModJson.str(manifest, "id");
        name = ModJson.str(manifest, "name");
        version = ModJson.str(manifest, "version");
        about = ModJson.str(manifest, "about");
        category = ModJson.str(manifest, "category");
        runtime = ModJson.str(manifest, "runtime");
        List<String> a = new ArrayList<>();
        List<Object> al = ModJson.list(manifest.get("authors"));
        if (al != null) {
            for (Object o : al) {
                a.add(String.valueOf(o));
            }
        }
        authors = a;
    }

    Setting setting(String key) {
        for (Setting s : settings) {
            if (s.key.equals(key)) {
                return s;
            }
        }
        return null;
    }

    /** The features this build offers (the ROM file, else the bring-up fallback). */
    static Set<String> servedFeatures() {
        File f = new File(FEATURES_FILE);
        Set<String> out = new LinkedHashSet<>();
        if (f.isFile()) {
            try {
                for (String l : Files.readAllLines(f.toPath(), StandardCharsets.UTF_8)) {
                    l = l.trim();
                    if (!l.isEmpty() && !l.startsWith("#")) {
                        out.add(l);
                    }
                }
                return out;
            } catch (IOException e) {
                // fall through to the fallback
            }
        }
        out.addAll(FALLBACK_FEATURES);
        return out;
    }

    // ---- Parsing + validation ------------------------------------------------------------------

    static ModuleSpec parse(ModulePackage pkg, Set<String> features) throws Invalid {
        return parse(pkg.text("abyss-app.nix"), pkg.files.keySet(), pkg::text, features);
    }

    interface FileText {
        String text(String path);
    }

    static ModuleSpec parse(String manifestSrc, Set<String> files, FileText read,
            Set<String> features) throws Invalid {
        if (manifestSrc == null) {
            throw new Invalid("abyss-app.nix: missing (not an Abyss app)");
        }
        NixParser.Result r = nix("abyss-app.nix", manifestSrc);
        Map<String, Object> m = r.root;
        Ctx c = new Ctx("abyss-app.nix", r);
        for (String k : m.keySet()) {
            if (!MANIFEST_KEYS.contains(k) && !k.startsWith("x-")) {
                throw c.err(k, k.equals("caps") ? "'caps' is derived from what the app uses; "
                        + "don't write it" : "unknown key '" + k + "'");
            }
        }
        String id = c.reqStr(m, "id");
        if (!ID.matcher(id).matches()) {
            throw c.err("id", "id must match [a-z0-9][a-z0-9-]{0,31}");
        }
        String name = c.reqStr(m, "name");
        if (name.length() > 24) {
            throw c.err("name", "name is longer than 24 characters");
        }
        if (!SEMVER.matcher(c.reqStr(m, "version")).matches()) {
            throw c.err("version", "version must be semver (1.2.3)");
        }
        Object api = m.get("api");
        if (!(api instanceof Long)) {
            throw c.err("api", "api must be an integer");
        }
        if (!API_LEVELS.contains(api)) {
            throw c.err("api", "needs app API " + api + "; this IchthysOS has " + API_LEVELS
                    + " (needs a newer IchthysOS)");
        }
        String cat = c.reqStr(m, "category");
        if (!CATEGORIES.contains(cat)) {
            throw c.err("category", "unknown category '" + cat + "' (one of " + CATEGORIES + ")");
        }
        Object rt = m.containsKey("runtime") ? m.get("runtime") : "script";
        if (!"pure".equals(rt)) {
            throw c.err("runtime", "runtime \"" + rt + "\": only declarative (\"pure\") modules "
                    + "can be installed from a file in this IchthysOS; script and container "
                    + "modules come with Termux staging (v1.1/v1.2)");
        }
        String about = c.reqStr(m, "about");
        if (about.length() > 120) {
            throw c.err("about", "about is longer than 120 characters");
        }
        for (String k : new String[] {"scripts", "connectors", "secrets", "actions", "oci",
                "fetch", "tiles", "accepts", "data", "events"}) {
            Object v = m.get(k);
            boolean empty = v == null || v instanceof List && ((List<?>) v).isEmpty()
                    || v instanceof Map && ((Map<?, ?>) v).isEmpty();
            if (!empty) {
                throw c.err(k, "'" + k + "' is not supported for a pure module in this "
                        + "IchthysOS");
            }
        }
        // requires
        Set<String> required = new LinkedHashSet<>();
        Map<String, Object> req = c.optMap(m, "requires");
        if (req != null) {
            for (String k : req.keySet()) {
                if (!k.equals("abyss") && !k.equals("features")) {
                    throw c.err("requires." + k, "unknown key 'requires." + k + "'");
                }
            }
            Object ra = req.get("abyss");
            if (ra != null) {
                if (!(ra instanceof Long) || (Long) ra < 1) {
                    throw c.err("requires.abyss", "requires.abyss must be an integer ≥ 1");
                }
                if ((Long) ra > Collections.max(API_LEVELS)) {
                    throw c.err("requires.abyss", "needs app API " + ra
                            + " (needs a newer IchthysOS)");
                }
            }
            List<Object> fl = c.optList(req, "features", "requires.features");
            if (fl != null) {
                for (Object f : fl) {
                    if (!(f instanceof String) || !FEATURE.matcher((String) f).matches()) {
                        throw c.err("requires.features", "bad feature name '" + f + "'");
                    }
                    required.add((String) f);
                }
            }
        }
        for (String f : required) {
            // features == null: re-reading an installed module (it passed at install).
            if (features != null && !features.contains(f)) {
                throw c.err("requires.features", "needs `" + f
                        + "`; this IchthysOS doesn't have it yet");
            }
        }
        // settings
        List<Setting> settings = new ArrayList<>();
        Map<String, Object> sm = c.optMap(m, "settings");
        Set<String> envs = new HashSet<>();
        if (sm != null) {
            for (Map.Entry<String, Object> e : sm.entrySet()) {
                Setting s = setting(c, e.getKey(), e.getValue());
                if (!envs.add(s.env)) {
                    throw c.err("settings." + s.key, "two settings use the variable " + s.env);
                }
                settings.add(s);
            }
        }
        // icon (optional; must be in the tree)
        String icon = ModJson.str(m, "icon");
        if (icon != null && !files.contains(icon)) {
            throw c.err("icon", "icon '" + icon + "' is not in the module");
        }
        // capture
        boolean captures = false;
        Object cap = m.get("capture");
        if (cap != null) {
            Map<String, Object> cm = ModJson.map(cap);
            if (cm == null) {
                throw c.err("capture", "a pure module's capture must be { line = …; }");
            }
            for (String k : cm.keySet()) {
                if (!k.equals("line") && !k.equals("kind") && !k.equals("fields")) {
                    throw c.err("capture." + k, "unknown key 'capture." + k + "'");
                }
            }
            c.reqStr(cm, "line", "capture.line");
            if (cm.containsKey("kind") || cm.containsKey("fields")) {
                needFeature(c, required, "capture:fields", "capture");
            }
            String kind = ModJson.str(cm, "kind");
            if (cm.containsKey("kind") && (kind == null
                    || !Pattern.matches("[a-z][a-z0-9-]{0,31}", kind))) {
                throw c.err("capture.kind", "capture.kind must match [a-z][a-z0-9-]{0,31}");
            }
            Map<String, Object> fields = c.optMap(cm, "fields", "capture.fields");
            if (fields != null) {
                for (Map.Entry<String, Object> e : fields.entrySet()) {
                    if (!(e.getValue() instanceof String)) {
                        throw c.err("capture.fields." + e.getKey(), "a capture field is a string");
                    }
                }
            }
            captures = true;
        }
        // menu
        String menuPath = c.reqStr(m, "menu");
        String menuSrc = files.contains(menuPath) ? read.text(menuPath) : null;
        if (menuSrc == null) {
            throw c.err("menu", "menu '" + menuPath + "' is not in the module");
        }
        NixParser.Result mr = nix(menuPath, menuSrc);
        MenuCheck mc = new MenuCheck(new Ctx(menuPath, mr), settings, required);
        mc.check(mr.root);
        // Holes in the capture: $stream.* and the like are checked with the menu's names.
        if (cap != null) {
            mc.checkHoles(c, "capture", cap, Collections.singleton("stream"));
        }

        // Normalised installed form (contract §3): defaults made explicit.
        Map<String, Object> norm = new LinkedHashMap<>(m);
        norm.put("runtime", "pure");
        if (sm != null) {
            Map<String, Object> ns = new LinkedHashMap<>();
            for (Setting s : settings) {
                Map<String, Object> o = new LinkedHashMap<>(ModJson.map(sm.get(s.key)));
                o.put("env", s.env);
                o.put("optional", s.optional);
                if ("enum".equals(s.type)) {
                    List<Object> ch = new ArrayList<>();
                    for (String[] cv : s.choices) {
                        Map<String, Object> co = new LinkedHashMap<>();
                        co.put("value", cv[0]);
                        co.put("label", cv[1]);
                        ch.add(co);
                    }
                    o.put("choices", ch);
                }
                if ("number".equals(s.type)) {
                    o.put("step", s.step);
                    o.put("integer", s.integer);
                }
                if ("text".equals(s.type) || "secret".equals(s.type)) {
                    o.put("maxLength", (long) s.maxLength);
                }
                ns.put(s.key, o);
            }
            norm.put("settings", ns);
        }
        ModuleSpec spec = new ModuleSpec(norm, mr.root, settings, mc.netSettings, mc.netLiteral,
                mc.streams, mc.loads, captures);
        norm.put("caps", spec.caps(null));
        return spec;
    }

    private static void needFeature(Ctx c, Set<String> required, String f, String where)
            throws Invalid {
        if (!required.contains(f)) {
            throw c.err(where, "uses `" + f + "`: add \"" + f + "\" to requires.features");
        }
    }

    private static NixParser.Result nix(String file, String src) throws Invalid {
        try {
            return NixParser.parse(src);
        } catch (NixParseException e) {
            throw new Invalid(file + ":" + e.getMessage());
        }
    }

    private static Setting setting(Ctx c, String key, Object v) throws Invalid {
        String p = "settings." + key;
        if (!SETTING_KEY.matcher(key).matches()) {
            throw c.err(p, "setting name '" + key + "' must match [a-zA-Z][a-zA-Z0-9]*");
        }
        Map<String, Object> o = ModJson.map(v);
        if (o == null) {
            throw c.err(p, "a setting is { type; label; … }");
        }
        for (String k : o.keySet()) {
            if (!SETTING_KEYS.contains(k)) {
                throw c.err(p + "." + k, "unknown key '" + k + "' in setting '" + key + "'");
            }
        }
        Setting s = new Setting();
        s.key = key;
        s.type = c.reqStr(o, "type", p + ".type");
        if (!Arrays.asList("text", "number", "bool", "enum", "secret").contains(s.type)) {
            throw c.err(p + ".type", "type must be text, number, bool, enum or secret");
        }
        s.label = c.reqStr(o, "label", p + ".label");
        if (s.label.length() > 32) {
            throw c.err(p + ".label", "label is longer than 32 characters");
        }
        s.help = ModJson.str(o, "help");
        if (s.help != null && s.help.length() > 120) {
            throw c.err(p + ".help", "help is longer than 120 characters");
        }
        s.optional = Boolean.TRUE.equals(o.get("optional"));
        s.env = o.containsKey("env") ? ModJson.str(o, "env") : upperSnake(key);
        if (s.env == null || !ENV.matcher(s.env).matches() || RESERVED_ENV.contains(s.env)
                || s.env.startsWith("ABYSS_") || s.env.startsWith("LD_")) {
            throw c.err(p + ".env", "variable name '" + s.env + "' is not allowed");
        }
        s.min = num(o.get("min"));
        s.max = num(o.get("max"));
        if (o.containsKey("step")) {
            Double st = num(o.get("step"));
            if (st == null || st <= 0) {
                throw c.err(p + ".step", "step must be a positive number");
            }
            s.step = st;
        }
        s.integer = Boolean.TRUE.equals(o.get("integer"));
        s.unit = ModJson.str(o, "unit");
        if (s.unit != null && s.unit.length() > 8) {
            throw c.err(p + ".unit", "unit is longer than 8 characters");
        }
        if (o.get("minLength") instanceof Long) {
            s.minLength = (int) (long) (Long) o.get("minLength");
        }
        if (o.get("maxLength") instanceof Long) {
            long ml = (Long) o.get("maxLength");
            if (ml < 1 || ml > 1000) {
                throw c.err(p + ".maxLength", "maxLength must be 1..1000");
            }
            s.maxLength = (int) ml;
        }
        s.pattern = ModJson.str(o, "pattern");
        if (s.pattern != null) {
            try {
                Pattern.compile(s.pattern);
            } catch (PatternSyntaxException e) {
                throw c.err(p + ".pattern", "pattern is not a valid regular expression");
            }
        }
        if ("enum".equals(s.type)) {
            List<Object> ch = c.optList(o, "choices", p + ".choices");
            if (ch == null || ch.size() < 2 || ch.size() > 20) {
                throw c.err(p + ".choices", "an enum needs 2 to 20 choices");
            }
            for (Object x : ch) {
                if (x instanceof String) {
                    s.choices.add(new String[] {(String) x, (String) x});
                } else if (ModJson.map(x) != null && ModJson.str(ModJson.map(x), "value") != null
                        && ModJson.str(ModJson.map(x), "label") != null
                        && ModJson.map(x).size() == 2) {
                    s.choices.add(new String[] {ModJson.str(ModJson.map(x), "value"),
                            ModJson.str(ModJson.map(x), "label")});
                } else {
                    throw c.err(p + ".choices", "a choice is \"value\" or { value; label; }");
                }
            }
        }
        if (o.containsKey("pick") || o.containsKey("test")) {
            throw c.err(p, "pick/test call a connector; a pure module has none");
        }
        if (o.containsKey("default")) {
            if (s.isSecret()) {
                throw c.err(p + ".default", "a secret has no default: nobody ships a key");
            }
            s.def = o.get("default");
            String bad = s.check(s.def);
            if (bad != null) {
                throw c.err(p + ".default", "default fails its own rules (" + bad + ")");
            }
        }
        return s;
    }

    static String upperSnake(String camel) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < camel.length(); i++) {
            char ch = camel.charAt(i);
            if (Character.isUpperCase(ch) && i > 0) {
                sb.append('_');
            }
            sb.append(Character.toUpperCase(ch));
        }
        return sb.toString();
    }

    private static Double num(Object o) {
        return o instanceof Long ? Double.valueOf((Long) o)
                : o instanceof Double ? (Double) o : null;
    }

    static String fmt(Double d) {
        if (d == null) {
            return "…";
        }
        return d == Math.rint(d) ? Long.toString(d.longValue()) : d.toString();
    }

    /** Error positions: NixParser records "a.b[0].c" → line:col. */
    private static final class Ctx {
        final String file;
        final NixParser.Result r;

        Ctx(String file, NixParser.Result r) {
            this.file = file;
            this.r = r;
        }

        Invalid err(String path, String msg) {
            String p = path;
            String where = "";
            while (p != null && !p.isEmpty()) {
                where = r.where(p);
                if (!where.isEmpty()) {
                    break;
                }
                int cut = Math.max(p.lastIndexOf('.'), p.lastIndexOf('['));
                p = cut > 0 ? p.substring(0, cut) : null;
            }
            return new Invalid(file + (where.isEmpty() ? "" : ":" + where) + ": " + msg);
        }

        String reqStr(Map<String, Object> m, String k) throws Invalid {
            return reqStr(m, k, k);
        }

        String reqStr(Map<String, Object> m, String k, String path) throws Invalid {
            Object v = m.get(k);
            if (!(v instanceof String) || ((String) v).isEmpty()) {
                throw err(path, (v == null ? "missing " : "") + "'" + k + "' must be a string");
            }
            return (String) v;
        }

        Map<String, Object> optMap(Map<String, Object> m, String k) throws Invalid {
            return optMap(m, k, k);
        }

        Map<String, Object> optMap(Map<String, Object> m, String k, String path)
                throws Invalid {
            Object v = m.get(k);
            if (v == null) {
                return null;
            }
            if (ModJson.map(v) == null) {
                throw err(path, "'" + k + "' must be an attribute set");
            }
            return ModJson.map(v);
        }

        List<Object> optList(Map<String, Object> m, String k, String path) throws Invalid {
            Object v = m.get(k);
            if (v == null) {
                return null;
            }
            if (ModJson.list(v) == null) {
                throw err(path, "'" + k + "' must be a list");
            }
            return ModJson.list(v);
        }
    }

    /** menu.nix rules (contract §5). */
    private static final class MenuCheck {
        final Ctx c;
        final List<Setting> settings;
        final Set<String> required;
        final Set<String> netSettings = new LinkedHashSet<>();
        final Set<String> netLiteral = new LinkedHashSet<>();
        final Set<String> asNames = new HashSet<>();
        boolean streams;
        boolean loads;
        Map<String, Object> screens;

        MenuCheck(Ctx c, List<Setting> settings, Set<String> required) {
            this.c = c;
            this.settings = settings;
            this.required = required;
        }

        void check(Map<String, Object> root) throws Invalid {
            for (String k : root.keySet()) {
                if (!k.equals("menu") && !k.equals("start") && !k.equals("screens")
                        && !k.startsWith("x-")) {
                    throw c.err(k, "unknown key '" + k + "'");
                }
            }
            if (!Long.valueOf(1).equals(root.get("menu"))) {
                throw c.err("menu", "menu = 1; is required (menu api 1)");
            }
            screens = c.optMap(root, "screens");
            if (screens == null || screens.isEmpty()) {
                throw c.err("screens", "a menu needs at least one screen");
            }
            String start = c.reqStr(root, "start");
            if (!screens.containsKey(start)) {
                throw c.err("start", "start screen '" + start + "' does not exist");
            }
            // First pass: collect `as` names so holes on later screens can be checked.
            for (Map.Entry<String, Object> e : screens.entrySet()) {
                Map<String, Object> s = ModJson.map(e.getValue());
                if (s == null) {
                    throw c.err("screens." + e.getKey(), "a screen is an attribute set");
                }
                Object items = s.get("items");
                Map<String, Object> im = ModJson.map(items);
                if (im != null && ModJson.map(im.get("row")) != null) {
                    String as = ModJson.str(ModJson.map(im.get("row")), "as");
                    if (as != null) {
                        addAs(as, "screens." + e.getKey() + ".items.row.as");
                    }
                }
                List<Object> il = ModJson.list(items);
                if (il != null) {
                    for (int i = 0; i < il.size(); i++) {
                        String as = ModJson.str(ModJson.map(il.get(i)), "as");
                        if (as != null) {
                            addAs(as, "screens." + e.getKey() + ".items[" + i + "].as");
                        }
                    }
                }
            }
            for (Map.Entry<String, Object> e : screens.entrySet()) {
                screen(e.getKey(), ModJson.map(e.getValue()));
            }
        }

        void addAs(String as, String path) throws Invalid {
            if (!Pattern.matches("[a-z][a-z0-9_]{0,31}", as) || RESERVED_NS.contains(as)) {
                throw c.err(path, "as = \"" + as + "\" is not allowed (lower case, not one of "
                        + RESERVED_NS + ")");
            }
            needFeature(c, required, "menu:get", path);
            asNames.add(as);
        }

        void screen(String name, Map<String, Object> s) throws Invalid {
            String p = "screens." + name;
            if (!Pattern.matches("[a-z][a-z0-9_-]{0,31}", name)) {
                throw c.err(p, "screen name '" + name + "' must match [a-z][a-z0-9_-]{0,31}");
            }
            for (String k : s.keySet()) {
                if (!SCREEN_KEYS.contains(k) && !k.startsWith("x-")) {
                    throw c.err(p + "." + k, "unknown key '" + k + "' on screen '" + name + "'");
                }
            }
            String kind = s.containsKey("kind") ? c.reqStr(s, "kind", p + ".kind") : "list";
            if (!SCREEN_KINDS.contains(kind)) {
                throw c.err(p + ".kind", "unknown screen kind '" + kind + "'");
            }
            if (!"list".equals(kind) && !"text".equals(kind)) {
                throw c.err(p + ".kind", "'" + kind + "' screens run something; a pure module "
                        + "can't use them yet");
            }
            Set<String> extra = new HashSet<>(asNames);
            extra.add("stream");
            boolean hasGet = s.containsKey("get");
            if (hasGet) {
                needFeature(c, required, "menu:get", p + ".get");
                loads = true;
                Map<String, Object> g = c.optMap(s, "get", p + ".get");
                for (String k : g.keySet()) {
                    if (!GET_KEYS.contains(k)) {
                        throw c.err(p + ".get." + k, "unknown key '" + k + "' in get");
                    }
                }
                url(p + ".get.url", c.reqStr(g, "url", p + ".get.url"), extra);
                for (String d : new String[] {"every"}) {
                    String v = ModJson.str(g, d);
                    if (g.containsKey(d) && (v == null || !DURATION.matcher(v).matches())) {
                        throw c.err(p + ".get." + d, d + " must be a duration like \"30s\"");
                    }
                }
                pointer(g, "refreshAt", p + ".get.refreshAt");
                Map<String, Object> page = c.optMap(g, "page", p + ".get.page");
                if (page != null) {
                    for (String k : page.keySet()) {
                        if (!Arrays.asList("param", "value", "label").contains(k)) {
                            throw c.err(p + ".get.page." + k, "unknown key '" + k + "' in page");
                        }
                    }
                    if (!Pattern.matches("[A-Za-z0-9_.-]{1,32}",
                            c.reqStr(page, "param", p + ".get.page.param"))) {
                        throw c.err(p + ".get.page.param", "page.param is a query name");
                    }
                    pointer(page, "value", p + ".get.page.value");
                    if (!page.containsKey("value")) {
                        throw c.err(p + ".get.page", "page needs value (a JSON pointer)");
                    }
                }
                extra.add("data");
            }
            for (String k : new String[] {"title", "sub", "empty", "error", "guide"}) {
                if (s.containsKey(k)) {
                    holes(p + "." + k, c.reqStr(s, k, p + "." + k), extra);
                }
            }
            if ("text".equals(kind)) {
                List<Object> lines = c.optList(s, "lines", p + ".lines");
                if (lines == null) {
                    throw c.err(p, "a text screen needs lines");
                }
                for (int i = 0; i < lines.size(); i++) {
                    String lp = p + ".lines[" + i + "]";
                    Object l = lines.get(i);
                    if (l instanceof String) {
                        holes(lp, (String) l, extra);
                        continue;
                    }
                    Map<String, Object> lm = ModJson.map(l);
                    if (lm == null) {
                        throw c.err(lp, "a line is a string or { text; when?; unless?; }");
                    }
                    for (String k : lm.keySet()) {
                        if (!Arrays.asList("text", "when", "unless").contains(k)) {
                            throw c.err(lp + "." + k, "unknown key '" + k + "' in a line");
                        }
                    }
                    holes(lp + ".text", c.reqStr(lm, "text", lp + ".text"), extra);
                    for (String k : new String[] {"when", "unless"}) {
                        if (lm.containsKey(k)) {
                            needFeature(c, required, "menu:get", lp + "." + k);
                            String h = c.reqStr(lm, k, lp + "." + k);
                            if (!HOLE.matcher(h).matches()) {
                                throw c.err(lp + "." + k, k + " is one hole, e.g. \"$data.item\"");
                            }
                            holes(lp + "." + k, h, extra);
                        }
                    }
                }
                if (s.containsKey("items")) {
                    throw c.err(p + ".items", "a text screen has lines, not items");
                }
                return;
            }
            Object items = s.get("items");
            if (ModJson.map(items) != null) {
                Map<String, Object> im = ModJson.map(items);
                for (String k : im.keySet()) {
                    if (!Arrays.asList("from", "row", "order").contains(k)) {
                        throw c.err(p + ".items." + k, "unknown key '" + k + "' in items");
                    }
                }
                String from = c.reqStr(im, "from", p + ".items.from");
                if (from.startsWith("$data")) {
                    if (!hasGet) {
                        throw c.err(p + ".items.from", "$data needs a get on this screen");
                    }
                    holes(p + ".items.from", from, extra);
                    Map<String, Object> row = c.optMap(im, "row", p + ".items.row");
                    if (row == null) {
                        throw c.err(p + ".items", "a list from $data needs a row template");
                    }
                    Set<String> rx = new HashSet<>(extra);
                    rx.add("row");
                    item(p + ".items.row", row, rx, true);
                } else if (from.startsWith("$state.")) {
                    needFeature(c, required, "menu:from-state", p + ".items.from");
                } else {
                    throw c.err(p + ".items.from", "items.from is $data.… or $state.…");
                }
                String order = ModJson.str(im, "order");
                if (im.containsKey("order") && !"reverse".equals(order)
                        && !"as-is".equals(order)) {
                    throw c.err(p + ".items.order", "order is \"as-is\" or \"reverse\"");
                }
            } else {
                List<Object> il = c.optList(s, "items", p + ".items");
                if (il == null) {
                    throw c.err(p, "a list screen needs items");
                }
                for (int i = 0; i < il.size(); i++) {
                    Map<String, Object> it = ModJson.map(il.get(i));
                    if (it == null) {
                        throw c.err(p + ".items[" + i + "]", "an item is an attribute set");
                    }
                    item(p + ".items[" + i + "]", it, extra, false);
                }
            }
        }

        void item(String p, Map<String, Object> it, Set<String> ns, boolean row)
                throws Invalid {
            for (String k : it.keySet()) {
                if (!ITEM_KEYS.contains(k) && !k.startsWith("x-")) {
                    throw c.err(p + "." + k, "unknown key '" + k + "' in an item");
                }
            }
            String label = c.reqStr(it, "label", p + ".label");
            holes(p + ".label", label, ns);
            if (!label.contains("$") && label.length() > 28) {
                throw c.err(p + ".label", "label is longer than 28 characters");
            }
            if (it.containsKey("sub")) {
                holes(p + ".sub", c.reqStr(it, "sub", p + ".sub"), ns);
            }
            String action = null;
            for (String a : ITEM_ACTIONS) {
                if (it.containsKey(a)) {
                    if (action != null) {
                        throw c.err(p + "." + a, "an item has one action ('" + action
                                + "' and '" + a + "')");
                    }
                    action = a;
                }
            }
            if (action == null) {
                throw c.err(p, "an item needs one of " + ITEM_ACTIONS);
            }
            if (it.containsKey("as") && !"goto".equals(action)) {
                throw c.err(p + ".as", "as names the row passed on by goto");
            }
            switch (action) {
                case "run":
                    throw c.err(p + ".run", "run starts a script; a pure module has none");
                case "goto": {
                    String g = c.reqStr(it, "goto", p + ".goto");
                    if (!screens.containsKey(g)) {
                        throw c.err(p + ".goto", "screen '" + g + "' does not exist");
                    }
                    break;
                }
                case "back":
                case "battery":
                    break;
                case "media": {
                    String v = c.reqStr(it, "media", p + ".media");
                    if (!Arrays.asList("play", "pause", "toggle", "next", "prev").contains(v)) {
                        throw c.err(p + ".media", "media is play, pause, toggle, next or prev");
                    }
                    break;
                }
                case "stream":
                    stream(p + ".stream", c.optMap(it, "stream", p + ".stream"), ns);
                    break;
                default:
                    if (it.get(action) instanceof String) {
                        holes(p + "." + action, (String) it.get(action), ns);
                    }
            }
            if (it.containsKey("reply")) {
                String rp = c.reqStr(it, "reply", p + ".reply");
                if (!Pattern.matches("(phrase:[a-z0-9_-]+|buzz:[.\\- ]{1,15}|bell:(info|done|"
                        + "needs-you))", rp)) {
                    throw c.err(p + ".reply", "reply is phrase:ID, buzz:PATTERN or bell:LEVEL");
                }
            }
            if (it.containsKey("args")) {
                throw c.err(p + ".args", "args go to a script; a pure module has none");
            }
        }

        void stream(String p, Map<String, Object> st, Set<String> ns) throws Invalid {
            needFeature(c, required, "media:stream", p);
            streams = true;
            for (String k : st.keySet()) {
                if (!STREAM_KEYS.contains(k)) {
                    throw c.err(p + "." + k, "unknown key '" + k + "' in stream");
                }
            }
            String proto = st.containsKey("protocol")
                    ? c.reqStr(st, "protocol", p + ".protocol") : "http";
            if (!STREAM_PROTOCOLS.contains(proto)) {
                throw c.err(p + ".protocol", "unknown stream protocol '" + proto + "'");
            }
            needFeature(c, required, "media:stream:" + proto, p + ".protocol");
            if (st.containsKey("url") == st.containsKey("formats")) {
                throw c.err(p, "a stream has url or formats (one of them)");
            }
            if (st.containsKey("url")) {
                url(p + ".url", c.reqStr(st, "url", p + ".url"), ns);
            }
            for (String k : new String[] {"formats", "format", "prebuffer", "songs", "tag",
                    "title"}) {
                if (st.containsKey(k)) {
                    String v = c.reqStr(st, k, p + "." + k);
                    holes(p + "." + k, v, ns);
                    if (k.equals("songs")) {
                        url(p + "." + k, v, ns);
                    }
                }
            }
            if (st.containsKey("show")) {
                String sh = c.reqStr(st, "show", p + ".show");
                Map<String, Object> target = ModJson.map(screens.get(sh));
                if (target == null || !"text".equals(target.get("kind"))) {
                    throw c.err(p + ".show", "show names a text screen of this menu");
                }
            }
        }

        void pointer(Map<String, Object> m, String k, String p) throws Invalid {
            if (!m.containsKey(k)) {
                return;
            }
            String v = c.reqStr(m, k, p);
            if (!v.startsWith("/")) {
                throw c.err(p, k + " is a JSON pointer (\"/item/ends_at_ms\")");
            }
        }

        /** A URL template: a literal http(s) origin, a $settings hole, or a link hole. */
        void url(String p, String v, Set<String> ns) throws Invalid {
            holes(p, v, ns);
            Matcher h = HOLE.matcher(v);
            if (h.lookingAt()) {
                if ("settings".equals(h.group(1))) {
                    netSettings.add(h.group(2).substring(1));
                }
                // $station.now_url etc: a link the server handed back; same-origin rule.
                return;
            }
            String o = origin(v);
            if (o == null) {
                throw c.err(p, "a URL starts with http(s)://host or a hole");
            }
            netLiteral.add(o);
        }

        void holes(String p, String v, Set<String> ns) throws Invalid {
            Matcher h = HOLE.matcher(v);
            while (h.find()) {
                String n = h.group(1);
                String path = h.group(2);
                switch (n) {
                    case "settings": {
                        String key = path.length() > 1 ? path.substring(1) : "";
                        Setting s = null;
                        for (Setting x : settings) {
                            if (x.key.equals(key)) {
                                s = x;
                            }
                        }
                        if (s == null) {
                            throw c.err(p, "no setting '" + key + "' (" + v + ")");
                        }
                        if (s.isSecret()) {
                            throw c.err(p, "'" + key + "' is a secret and can never be shown "
                                    + "or sent by a menu");
                        }
                        break;
                    }
                    case "state":
                    case "battery":
                    case "time":
                    case "value":
                        break;
                    default:
                        if (!ns.contains(n)) {
                            throw c.err(p, "unknown hole $" + n + " here (" + v + ")");
                        }
                        if ("stream".equals(n) || "data".equals(n) || "row".equals(n)
                                || asNames.contains(n)) {
                            // shapes come from the server; checked when drawn
                            break;
                        }
                }
            }
        }

        void checkHoles(Ctx mc, String p, Object v, Set<String> ns) throws Invalid {
            if (v instanceof String) {
                // holes() reports against the menu ctx; capture lives in abyss-app.nix.
                try {
                    holes(p, (String) v, ns);
                } catch (Invalid e) {
                    throw mc.err(p, e.getMessage().replaceFirst("^[^ ]+ ", ""));
                }
            } else if (ModJson.map(v) != null) {
                for (Map.Entry<String, Object> e : ModJson.map(v).entrySet()) {
                    checkHoles(mc, p + "." + e.getKey(), e.getValue(), ns);
                }
            }
        }
    }

    // ---- Capabilities ----------------------------------------------------------------------

    /** scheme://host[:port] of an http(s) URL (lower case), or null. */
    static String origin(String url) {
        if (url == null) {
            return null;
        }
        Matcher m = Pattern.compile("(https?)://([A-Za-z0-9.-]+|\\[[0-9A-Fa-f:.]+\\])"
                + "(:[0-9]{1,5})?(?:[/?#].*)?").matcher(url.trim());
        if (!m.matches()) {
            return null;
        }
        return m.group(1).toLowerCase(Locale.ROOT) + "://" + m.group(2).toLowerCase(Locale.ROOT)
                + (m.group(3) == null ? "" : m.group(3));
    }

    /** True for a host on the home network: private IPv4, loopback, .local/.lan/.home.arpa. */
    static boolean isLan(String origin) {
        if (origin == null) {
            return false;
        }
        String host;
        try {
            host = URI.create(origin).getHost();
        } catch (IllegalArgumentException e) {
            return false;
        }
        if (host == null) {
            return false;
        }
        host = host.toLowerCase(Locale.ROOT);
        if (host.endsWith(".local") || host.endsWith(".lan") || host.endsWith(".home.arpa")
                || host.equals("localhost")) {
            return true;
        }
        Matcher m = Pattern.compile("([0-9]{1,3})\\.([0-9]{1,3})\\.([0-9]{1,3})\\.[0-9]{1,3}")
                .matcher(host);
        if (!m.matches()) {
            return false;
        }
        int a = Integer.parseInt(m.group(1));
        int b = Integer.parseInt(m.group(2));
        return a == 10 || a == 127 || a == 192 && b == 168 || a == 172 && b >= 16 && b <= 31
                || a == 169 && b == 254;
    }

    /** The network origins this module may reach with these setting values. */
    Set<String> netOrigins(Map<String, Object> values) {
        Set<String> out = new LinkedHashSet<>(netLiteral);
        for (String k : netSettings) {
            Object v = values == null ? null : values.get(k);
            if (v == null && setting(k) != null) {
                v = setting(k).def;
            }
            String o = v instanceof String ? origin((String) v) : null;
            if (o != null) {
                out.add(o);
            }
        }
        return out;
    }

    /** Derived caps in header words; null values = with the defaults ("$settings.K" kept). */
    List<String> caps(Map<String, Object> values) {
        List<String> caps = new ArrayList<>();
        for (String k : netSettings) {
            if (values == null) {
                caps.add("net $settings." + k + " (links from it: same origin only)");
            }
        }
        if (values != null) {
            for (String o : netOrigins(values)) {
                caps.add("net " + o + (isLan(o) ? " lan" : "")
                        + " (links from it: same origin only)");
            }
        } else {
            for (String o : netLiteral) {
                caps.add("net " + o + (isLan(o) ? " lan" : ""));
            }
        }
        if (loads) {
            caps.add("lease wifi while a screen loads");
        }
        if (streams) {
            caps.add("lease wifi bursts while playing");
            caps.add("needs media");
        }
        if (captures) {
            caps.add("post capture");
        }
        return caps;
    }

    /** The install sheet's list, in plain words. */
    List<Permission> permissions(Map<String, Object> values) {
        List<Permission> out = new ArrayList<>();
        for (String o : netOrigins(values)) {
            out.add(new Permission(isLan(o)
                    ? "Network: only " + o + ", on your home network. Links it hands back "
                            + "must point to the same address."
                    : "Network: only " + o + ", on the internet. Links it hands back must "
                            + "point to the same address.",
                    "net " + o + (isLan(o) ? " lan" : "")));
        }
        if (streams) {
            out.add(new Permission("Wi-Fi in Abyss while it plays: a short burst about every "
                    + "15 seconds, not all the time.", "lease wifi bursts while playing"));
            out.add(new Permission("Plays audio through your headphones.", "needs media"));
        }
        if (loads) {
            out.add(new Permission("Wi-Fi in Abyss for a moment when you open one of its "
                    + "lists.", "lease wifi while a screen loads"));
        }
        if (captures) {
            out.add(new Permission("Saves the song you hear when you press Volume Down + Power "
                    + "(to Captures).", "post capture"));
        }
        out.add(new Permission("Runs no code of its own: Abyss draws its screens and plays its "
                + "audio. It can't read your files, other modules or your keys.",
                "runtime pure"));
        return out;
    }
}
