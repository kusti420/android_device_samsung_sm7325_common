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

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.MessageDigest;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.regex.Pattern;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;

/**
 * An Abyss app artifact ({@code .abyssapp}): a zip of one app directory plus its
 * {@code SIGNATURE}. See ~/abyss-design/reports/module-install-contract.md §2.
 *
 * <p>{@code SIGNATURE} = the tree listing ({@code #abyss-tree v1}, then one
 * {@code mode TAB sha256 TAB size TAB path} line per file, sorted by UTF-8 bytes) followed by an
 * OpenSSH signature over exactly the listing bytes, namespace {@code abyss-app}. The zip must hold
 * exactly the listed files (and SIGNATURE), each matching its listed size and hash: what is
 * installed is what was signed, whatever the zip's local and central headers claim.
 */
final class ModulePackage {
    static final String NAMESPACE = "abyss-app";
    static final String LISTING_HEADER = "#abyss-tree v1\n";
    static final int MAX_FILES = 256;
    static final int MAX_FILE_BYTES = 1 << 20;
    static final int MAX_TOTAL_BYTES = 4 << 20;
    private static final Pattern SEGMENT = Pattern.compile("[A-Za-z0-9._-]+");
    private static final Pattern LINE =
            Pattern.compile("(644|755)\t([0-9a-f]{64})\t([0-9]{1,10})\t(.+)");

    /** Not an installable artifact; the message is one plain sentence for the install sheet. */
    static final class Refused extends Exception {
        private static final long serialVersionUID = 1L;

        Refused(String msg) {
            super(msg);
        }
    }

    /** One listed file. */
    static final class Entry {
        final String path;
        final boolean executable;
        final byte[] data;

        Entry(String path, boolean executable, byte[] data) {
            this.path = path;
            this.executable = executable;
            this.data = data;
        }
    }

    /** Files in listing order. */
    final Map<String, Entry> files;
    /** The SIGNATURE file as shipped. */
    final byte[] signatureFile;
    /** SHA-256 (hex) of the listing bytes: the app's content hash. */
    final String treeSha256;
    final SshSig.Signer signer;

    private ModulePackage(Map<String, Entry> files, byte[] signatureFile, String treeSha256,
            SshSig.Signer signer) {
        this.files = files;
        this.signatureFile = signatureFile;
        this.treeSha256 = treeSha256;
        this.signer = signer;
    }

    String text(String path) {
        Entry e = files.get(path);
        return e == null ? null : new String(e.data, StandardCharsets.UTF_8);
    }

    /** Reads and checks an artifact. The stream is read to the end but not closed. */
    static ModulePackage read(InputStream in) throws IOException, Refused {
        Map<String, byte[]> raw = new LinkedHashMap<>();
        long total = 0;
        ZipInputStream z = new ZipInputStream(in);
        ZipEntry ze;
        while ((ze = z.getNextEntry()) != null) {
            String name = ze.getName();
            if (ze.isDirectory()) {
                checkPath(name.endsWith("/") ? name.substring(0, name.length() - 1) : name);
                continue;
            }
            checkPath(name);
            if (raw.containsKey(name)) {
                throw new Refused("The file has \"" + name + "\" twice.");
            }
            if (raw.size() > MAX_FILES) {
                throw new Refused("Too many files (at most " + MAX_FILES + ").");
            }
            byte[] data = readBounded(z, MAX_FILE_BYTES, name);
            total += data.length;
            if (total > MAX_TOTAL_BYTES + MAX_FILE_BYTES) {
                throw new Refused("Too big (at most 4 MiB).");
            }
            raw.put(name, data);
        }
        byte[] sigFile = raw.remove("SIGNATURE");
        if (sigFile == null) {
            throw new Refused("Not signed: the file has no SIGNATURE.");
        }
        String sigText = new String(sigFile, StandardCharsets.UTF_8);
        int cut = sigText.indexOf(SshSig.BEGIN);
        if (cut < 0 || !sigText.startsWith(LISTING_HEADER)) {
            throw new Refused("SIGNATURE is not an Abyss app signature (#abyss-tree v1).");
        }
        byte[] listing = sigText.substring(0, cut).getBytes(StandardCharsets.UTF_8);
        Map<String, Entry> files = new LinkedHashMap<>();
        String prev = null;
        String[] lines = sigText.substring(LISTING_HEADER.length(), cut).split("\n", -1);
        for (int i = 0; i < lines.length; i++) {
            String ln = lines[i];
            if (ln.isEmpty() && i == lines.length - 1) {
                break;
            }
            java.util.regex.Matcher m = LINE.matcher(ln);
            if (!m.matches()) {
                throw new Refused("SIGNATURE line " + (i + 2) + " is malformed.");
            }
            String path = m.group(4);
            checkPath(path);
            if (prev != null && compareUtf8(prev, path) >= 0) {
                throw new Refused("SIGNATURE lists files out of order.");
            }
            prev = path;
            byte[] data = raw.remove(path);
            if (data == null) {
                throw new Refused("\"" + path + "\" is signed but missing from the file.");
            }
            if (data.length != Long.parseLong(m.group(3)) || !sha256(data).equals(m.group(2))) {
                throw new Refused("\"" + path + "\" was changed after it was signed.");
            }
            files.put(path, new Entry(path, "755".equals(m.group(1)), data));
        }
        if (!raw.isEmpty()) {
            throw new Refused("\"" + raw.keySet().iterator().next()
                    + "\" is in the file but not signed.");
        }
        if (files.size() > MAX_FILES || total > MAX_TOTAL_BYTES + sigFile.length) {
            throw new Refused("Too big (at most " + MAX_FILES + " files, 4 MiB).");
        }
        SshSig.Signer signer;
        try {
            signer = SshSig.verify(sigText.substring(cut), listing, NAMESPACE);
        } catch (SshSig.BadSignature e) {
            throw new Refused("Signature not valid: " + e.getMessage() + ".");
        }
        return new ModulePackage(files, sigFile, sha256(listing), signer);
    }

    private static void checkPath(String p) throws Refused {
        if (p.isEmpty() || p.length() > 200 || p.startsWith("/")) {
            throw new Refused("A file name in it is not allowed: \"" + p + "\".");
        }
        for (String s : p.split("/", -1)) {
            if (s.equals(".") || s.equals("..") || !SEGMENT.matcher(s).matches()) {
                throw new Refused("A file name in it is not allowed: \"" + p + "\".");
            }
        }
    }

    private static byte[] readBounded(InputStream in, int max, String name)
            throws IOException, Refused {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        int n;
        while ((n = in.read(buf)) > 0) {
            o.write(buf, 0, n);
            if (o.size() > max) {
                throw new Refused("\"" + name + "\" is too big (at most 1 MiB).");
            }
        }
        return o.toByteArray();
    }

    static String sha256(byte[] d) {
        try {
            byte[] h = MessageDigest.getInstance("SHA-256").digest(d);
            StringBuilder sb = new StringBuilder();
            for (byte b : h) {
                sb.append(String.format("%02x", b & 0xff));
            }
            return sb.toString();
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException(e);
        }
    }

    private static int compareUtf8(String a, String b) {
        byte[] x = a.getBytes(StandardCharsets.UTF_8);
        byte[] y = b.getBytes(StandardCharsets.UTF_8);
        for (int i = 0; i < Math.min(x.length, y.length); i++) {
            int c = (x[i] & 0xff) - (y[i] & 0xff);
            if (c != 0) {
                return c;
            }
        }
        return x.length - y.length;
    }
}
