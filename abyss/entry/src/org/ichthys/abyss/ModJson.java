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

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Small JSON reader/writer for the module store (manifest.json, menu.json, install.json).
 *
 * <p>Values are {@code Map<String, Object>} (insertion order kept), {@code List<Object>},
 * {@code String}, {@code Long}, {@code Double}, {@code Boolean} and {@link #NULL}: the same shapes
 * NixParser returns, so a parsed abyss-app.nix is written 1:1. Pure Java (no android.*, no
 * org.json, whose SDK stubs throw on the host) so the installer core runs in host tests.
 */
final class ModJson {
    /** JSON null. */
    static final Object NULL = new Object() {
        @Override
        public String toString() {
            return "null";
        }
    };

    private ModJson() {
    }

    // ---- Writing -------------------------------------------------------------------------------

    /** Pretty JSON (2-space indent), newline at the end. Nix null maps to JSON null. */
    static String write(Object v) {
        StringBuilder sb = new StringBuilder();
        write(sb, v, 0);
        sb.append('\n');
        return sb.toString();
    }

    @SuppressWarnings("unchecked")
    private static void write(StringBuilder sb, Object v, int ind) {
        if (v == null || v == NULL || "null".equals(String.valueOf(v)) && !(v instanceof String)) {
            sb.append("null");
        } else if (v instanceof String) {
            quote(sb, (String) v);
        } else if (v instanceof Boolean || v instanceof Long || v instanceof Integer) {
            sb.append(v);
        } else if (v instanceof Double) {
            double d = (Double) v;
            if (Double.isNaN(d) || Double.isInfinite(d)) {
                sb.append("null");
            } else if (d == Math.rint(d) && Math.abs(d) < 1e15) {
                sb.append((long) d).append(".0");
            } else {
                sb.append(d);
            }
        } else if (v instanceof Map) {
            Map<String, Object> m = (Map<String, Object>) v;
            if (m.isEmpty()) {
                sb.append("{}");
                return;
            }
            sb.append("{\n");
            int i = 0;
            for (Map.Entry<String, Object> e : m.entrySet()) {
                indent(sb, ind + 1);
                quote(sb, e.getKey());
                sb.append(": ");
                write(sb, e.getValue(), ind + 1);
                sb.append(++i < m.size() ? ",\n" : "\n");
            }
            indent(sb, ind);
            sb.append('}');
        } else if (v instanceof List) {
            List<Object> l = (List<Object>) v;
            if (l.isEmpty()) {
                sb.append("[]");
                return;
            }
            boolean flat = true;
            for (Object o : l) {
                flat &= !(o instanceof Map || o instanceof List);
            }
            if (flat) {
                sb.append('[');
                for (int i = 0; i < l.size(); i++) {
                    if (i > 0) {
                        sb.append(", ");
                    }
                    write(sb, l.get(i), ind);
                }
                sb.append(']');
                return;
            }
            sb.append("[\n");
            for (int i = 0; i < l.size(); i++) {
                indent(sb, ind + 1);
                write(sb, l.get(i), ind + 1);
                sb.append(i + 1 < l.size() ? ",\n" : "\n");
            }
            indent(sb, ind);
            sb.append(']');
        } else {
            quote(sb, String.valueOf(v));
        }
    }

    private static void indent(StringBuilder sb, int n) {
        for (int i = 0; i < n; i++) {
            sb.append("  ");
        }
    }

    static void quote(StringBuilder sb, String s) {
        sb.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"': sb.append("\\\""); break;
                case '\\': sb.append("\\\\"); break;
                case '\n': sb.append("\\n"); break;
                case '\r': sb.append("\\r"); break;
                case '\t': sb.append("\\t"); break;
                default:
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
            }
        }
        sb.append('"');
    }

    // ---- Reading -------------------------------------------------------------------------------

    /** Parses one JSON value; throws IllegalArgumentException with the offset on bad input. */
    static Object parse(String s) {
        Reader r = new Reader(s);
        r.ws();
        Object v = r.value(0);
        r.ws();
        if (r.pos != s.length()) {
            throw r.err("trailing characters");
        }
        return v;
    }

    private static final class Reader {
        final String s;
        int pos;

        Reader(String s) {
            this.s = s;
        }

        IllegalArgumentException err(String what) {
            return new IllegalArgumentException("JSON at " + pos + ": " + what);
        }

        void ws() {
            while (pos < s.length() && " \t\r\n".indexOf(s.charAt(pos)) >= 0) {
                pos++;
            }
        }

        Object value(int depth) {
            if (depth > 64) {
                throw err("nested too deep");
            }
            if (pos >= s.length()) {
                throw err("unexpected end");
            }
            char c = s.charAt(pos);
            if (c == '{') {
                pos++;
                Map<String, Object> m = new LinkedHashMap<>();
                ws();
                if (peek('}')) {
                    pos++;
                    return m;
                }
                while (true) {
                    ws();
                    if (!peek('"')) {
                        throw err("expected a key");
                    }
                    String k = string();
                    ws();
                    expect(':');
                    ws();
                    m.put(k, value(depth + 1));
                    ws();
                    if (peek(',')) {
                        pos++;
                        continue;
                    }
                    expect('}');
                    return m;
                }
            }
            if (c == '[') {
                pos++;
                List<Object> l = new ArrayList<>();
                ws();
                if (peek(']')) {
                    pos++;
                    return l;
                }
                while (true) {
                    ws();
                    l.add(value(depth + 1));
                    ws();
                    if (peek(',')) {
                        pos++;
                        continue;
                    }
                    expect(']');
                    return l;
                }
            }
            if (c == '"') {
                return string();
            }
            if (s.startsWith("true", pos)) {
                pos += 4;
                return Boolean.TRUE;
            }
            if (s.startsWith("false", pos)) {
                pos += 5;
                return Boolean.FALSE;
            }
            if (s.startsWith("null", pos)) {
                pos += 4;
                return NULL;
            }
            return number();
        }

        boolean peek(char c) {
            return pos < s.length() && s.charAt(pos) == c;
        }

        void expect(char c) {
            if (!peek(c)) {
                throw err("expected '" + c + "'");
            }
            pos++;
        }

        String string() {
            expect('"');
            StringBuilder sb = new StringBuilder();
            while (true) {
                if (pos >= s.length()) {
                    throw err("unterminated string");
                }
                char c = s.charAt(pos++);
                if (c == '"') {
                    return sb.toString();
                }
                if (c != '\\') {
                    sb.append(c);
                    continue;
                }
                if (pos >= s.length()) {
                    throw err("bad escape");
                }
                char e = s.charAt(pos++);
                switch (e) {
                    case '"': sb.append('"'); break;
                    case '\\': sb.append('\\'); break;
                    case '/': sb.append('/'); break;
                    case 'b': sb.append('\b'); break;
                    case 'f': sb.append('\f'); break;
                    case 'n': sb.append('\n'); break;
                    case 'r': sb.append('\r'); break;
                    case 't': sb.append('\t'); break;
                    case 'u':
                        if (pos + 4 > s.length()) {
                            throw err("bad \\u escape");
                        }
                        sb.append((char) Integer.parseInt(s.substring(pos, pos + 4), 16));
                        pos += 4;
                        break;
                    default:
                        throw err("bad escape");
                }
            }
        }

        Object number() {
            int start = pos;
            while (pos < s.length() && "+-0123456789.eE".indexOf(s.charAt(pos)) >= 0) {
                pos++;
            }
            String t = s.substring(start, pos);
            if (t.isEmpty()) {
                throw err("unexpected character");
            }
            try {
                if (t.indexOf('.') < 0 && t.indexOf('e') < 0 && t.indexOf('E') < 0) {
                    return Long.parseLong(t);
                }
                return Double.parseDouble(t);
            } catch (NumberFormatException e) {
                throw err("bad number '" + t + "'");
            }
        }
    }

    // ---- Access helpers ------------------------------------------------------------------------

    @SuppressWarnings("unchecked")
    static Map<String, Object> map(Object v) {
        return v instanceof Map ? (Map<String, Object>) v : null;
    }

    @SuppressWarnings("unchecked")
    static List<Object> list(Object v) {
        return v instanceof List ? (List<Object>) v : null;
    }

    static String str(Map<String, Object> m, String k) {
        Object v = m == null ? null : m.get(k);
        return v instanceof String ? (String) v : null;
    }
}
