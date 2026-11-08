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
import java.math.BigInteger;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.AlgorithmParameters;
import java.security.GeneralSecurityException;
import java.security.KeyFactory;
import java.security.MessageDigest;
import java.security.PublicKey;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;
import java.security.spec.ECParameterSpec;
import java.security.spec.ECPoint;
import java.security.spec.ECPublicKeySpec;
import java.security.spec.X509EncodedKeySpec;
import java.util.Arrays;
import java.util.Base64;

/**
 * Verifies an OpenSSH signature ("-----BEGIN SSH SIGNATURE-----", PROTOCOL.sshsig), the format
 * {@code ssh-keygen -Y sign} writes and Abyss app {@code SIGNATURE} files carry.
 *
 * <p>Key types: {@code ecdsa-sha2-nistp256} (works on every Android: SHA256withECDSA) and
 * {@code ssh-ed25519} (used when the platform's providers offer Ed25519; otherwise refused with
 * a clear message). Message hashes: sha256, sha512. Pure Java, so host tests run it too.
 */
final class SshSig {
    static final String BEGIN = "-----BEGIN SSH SIGNATURE-----";
    static final String END = "-----END SSH SIGNATURE-----";

    /** Who signed: the key in the signature, as OpenSSH prints it. */
    static final class Signer {
        final String keyType;
        /** "SHA256:" + unpadded base64 of SHA-256(public key blob), as ssh-keygen -l prints. */
        final String fingerprint;
        /** "ecdsa-sha2-nistp256 AAAA…", the authorized_keys / allowed_signers form. */
        final String publicKey;

        Signer(String keyType, String fingerprint, String publicKey) {
            this.keyType = keyType;
            this.fingerprint = fingerprint;
            this.publicKey = publicKey;
        }
    }

    /** A signature that does not verify, or a format/key type this phone cannot check. */
    static final class BadSignature extends Exception {
        private static final long serialVersionUID = 1L;

        BadSignature(String msg) {
            super(msg);
        }
    }

    private SshSig() {
    }

    /**
     * Checks {@code armored} over {@code message} in {@code namespace}; returns the signer.
     * The caller decides whether that signer is trusted.
     */
    static Signer verify(String armored, byte[] message, String namespace) throws BadSignature {
        byte[] blob = dearmor(armored);
        Buf b = new Buf(blob);
        if (!Arrays.equals(b.raw(6), "SSHSIG".getBytes(StandardCharsets.US_ASCII))) {
            throw new BadSignature("not an SSH signature");
        }
        if (b.u32() != 1) {
            throw new BadSignature("unknown SSH signature version");
        }
        byte[] pub = b.string();
        String ns = new String(b.string(), StandardCharsets.UTF_8);
        b.string(); // reserved
        String hashAlg = new String(b.string(), StandardCharsets.US_ASCII);
        byte[] sig = b.string();
        if (!ns.equals(namespace)) {
            throw new BadSignature("signed for \"" + ns + "\", not \"" + namespace + "\"");
        }
        byte[] h;
        try {
            h = MessageDigest.getInstance(
                    "sha512".equals(hashAlg) ? "SHA-512"
                            : "sha256".equals(hashAlg) ? "SHA-256" : "none").digest(message);
        } catch (GeneralSecurityException e) {
            throw new BadSignature("unknown hash " + hashAlg);
        }
        // The signed data (PROTOCOL.sshsig §3).
        ByteArrayOutputStream sd = new ByteArrayOutputStream();
        sd.write("SSHSIG".getBytes(StandardCharsets.US_ASCII), 0, 6);
        putString(sd, namespace.getBytes(StandardCharsets.UTF_8));
        putString(sd, new byte[0]);
        putString(sd, hashAlg.getBytes(StandardCharsets.US_ASCII));
        putString(sd, h);
        byte[] signed = sd.toByteArray();

        Buf pk = new Buf(pub);
        String keyType = new String(pk.string(), StandardCharsets.US_ASCII);
        Buf sb = new Buf(sig);
        String sigType = new String(sb.string(), StandardCharsets.US_ASCII);
        byte[] sigBlob = sb.string();
        if (!sigType.equals(keyType)) {
            throw new BadSignature("signature type " + sigType + " does not match key " + keyType);
        }
        boolean ok;
        try {
            if ("ecdsa-sha2-nistp256".equals(keyType)) {
                String curve = new String(pk.string(), StandardCharsets.US_ASCII);
                byte[] q = pk.string();
                if (!"nistp256".equals(curve) || q.length != 65 || q[0] != 4) {
                    throw new BadSignature("bad nistp256 key");
                }
                AlgorithmParameters ap = AlgorithmParameters.getInstance("EC");
                ap.init(new ECGenParameterSpec("secp256r1"));
                ECParameterSpec params = ap.getParameterSpec(ECParameterSpec.class);
                ECPoint w = new ECPoint(new BigInteger(1, Arrays.copyOfRange(q, 1, 33)),
                        new BigInteger(1, Arrays.copyOfRange(q, 33, 65)));
                PublicKey key = KeyFactory.getInstance("EC")
                        .generatePublic(new ECPublicKeySpec(w, params));
                Buf rs = new Buf(sigBlob);
                byte[] der = derSignature(new BigInteger(rs.string()), new BigInteger(rs.string()));
                Signature v = Signature.getInstance("SHA256withECDSA");
                v.initVerify(key);
                v.update(signed);
                ok = v.verify(der);
            } else if ("ssh-ed25519".equals(keyType)) {
                byte[] a = pk.string();
                if (a.length != 32 || sigBlob.length != 64) {
                    throw new BadSignature("bad ed25519 key or signature");
                }
                byte[] x509 = new byte[44];
                System.arraycopy(new byte[] {0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65,
                        0x70, 0x03, 0x21, 0x00}, 0, x509, 0, 12);
                System.arraycopy(a, 0, x509, 12, 32);
                PublicKey key;
                Signature v;
                try {
                    key = KeyFactory.getInstance("Ed25519")
                            .generatePublic(new X509EncodedKeySpec(x509));
                    v = Signature.getInstance("Ed25519");
                } catch (GeneralSecurityException e) {
                    throw new BadSignature("this phone can't check ed25519 signatures yet; "
                            + "sign with an ecdsa-sha2-nistp256 key");
                }
                v.initVerify(key);
                v.update(signed);
                ok = v.verify(sigBlob);
            } else {
                throw new BadSignature("key type " + keyType + " is not supported");
            }
        } catch (GeneralSecurityException | RuntimeException e) {
            throw new BadSignature("signature check failed: " + e.getMessage());
        }
        if (!ok) {
            throw new BadSignature("the signature does not match the files");
        }
        return new Signer(keyType, fingerprint(pub),
                keyType + " " + Base64.getEncoder().encodeToString(pub));
    }

    static String fingerprint(byte[] pubBlob) {
        try {
            byte[] d = MessageDigest.getInstance("SHA-256").digest(pubBlob);
            return "SHA256:" + Base64.getEncoder().withoutPadding().encodeToString(d);
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException(e);
        }
    }

    private static byte[] dearmor(String a) throws BadSignature {
        int s = a.indexOf(BEGIN);
        int e = a.indexOf(END);
        if (s < 0 || e < s) {
            throw new BadSignature("no SSH signature block");
        }
        String b64 = a.substring(s + BEGIN.length(), e).replaceAll("\\s+", "");
        try {
            return Base64.getDecoder().decode(b64);
        } catch (IllegalArgumentException ex) {
            throw new BadSignature("signature block is not base64");
        }
    }

    private static byte[] derSignature(BigInteger r, BigInteger s) {
        byte[] rb = r.toByteArray();
        byte[] sb = s.toByteArray();
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        o.write(0x30);
        o.write(4 + rb.length + sb.length);
        o.write(0x02);
        o.write(rb.length);
        o.write(rb, 0, rb.length);
        o.write(0x02);
        o.write(sb.length);
        o.write(sb, 0, sb.length);
        return o.toByteArray();
    }

    private static void putString(ByteArrayOutputStream o, byte[] v) {
        o.write(v.length >>> 24);
        o.write(v.length >>> 16);
        o.write(v.length >>> 8);
        o.write(v.length);
        o.write(v, 0, v.length);
    }

    /** SSH wire-format reader (uint32 + string). */
    private static final class Buf {
        private final ByteBuffer b;

        Buf(byte[] d) {
            b = ByteBuffer.wrap(d);
        }

        byte[] raw(int n) throws BadSignature {
            if (b.remaining() < n) {
                throw new BadSignature("truncated signature");
            }
            byte[] o = new byte[n];
            b.get(o);
            return o;
        }

        long u32() throws BadSignature {
            return ByteBuffer.wrap(raw(4)).getInt() & 0xffffffffL;
        }

        byte[] string() throws BadSignature {
            long n = u32();
            if (n > b.remaining()) {
                throw new BadSignature("truncated signature");
            }
            return raw((int) n);
        }
    }
}
