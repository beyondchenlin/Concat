// SPDX-License-Identifier: AGPL-3.0-or-later
package app.concat.editor;

import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

/** Versioned authenticated bytes only. No filesystem, UI, network or logging. */
final class BKCatCredentialPolicy {
    static final int MAX_SECRET_BYTES = 2560;
    static final int IV_BYTES = 12;
    static final int TAG_BITS = 128;
    private static final byte[] HEADER = { 'B', 'K', 'C', 'V', 1, IV_BYTES, 0, 0 };
    static final int MAX_ENVELOPE_BYTES = HEADER.length + IV_BYTES + MAX_SECRET_BYTES + TAG_BITS / 8;
    static final String ALIAS_PREFIX = "app.concat.editor.bkcat.credentials.v1.";

    private BKCatCredentialPolicy() {}

    static boolean validKeyId(String keyId) {
        if (keyId == null || keyId.length() != 64) return false;
        for (int i = 0; i < keyId.length(); i++) {
            char c = keyId.charAt(i);
            if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
        }
        return true;
    }

    static void validateSecret(byte[] secret) {
        if (secret == null || secret.length == 0 || secret.length > MAX_SECRET_BYTES) {
            throw new IllegalArgumentException("invalid-credential");
        }
        // Native Secret performs full UTF-8/text validation. Reject the most
        // hazardous byte values at the JNI boundary independently as well.
        for (byte b : secret) {
            int c = b & 255;
            if (c < 32 || c == 127) throw new IllegalArgumentException("invalid-credential");
        }
    }

    private static byte[] associatedData(String keyId) {
        if (!validKeyId(keyId)) throw new IllegalArgumentException("invalid-scope");
        return (ALIAS_PREFIX + keyId).getBytes(StandardCharsets.US_ASCII);
    }

    static byte[] seal(String keyId, byte[] secret, SecretKey key) throws GeneralSecurityException {
        validateSecret(secret);
        byte[] aad = associatedData(keyId);
        Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
        // AndroidKeyStore supplies a fresh IV. Never derive it from scope.
        cipher.init(Cipher.ENCRYPT_MODE, key);
        cipher.updateAAD(aad);
        byte[] iv = cipher.getIV();
        if (iv == null || iv.length != IV_BYTES) throw new GeneralSecurityException("invalid-iv");
        byte[] ciphertext = cipher.doFinal(secret);
        return ByteBuffer.allocate(HEADER.length + IV_BYTES + ciphertext.length)
                .put(HEADER).put(iv).put(ciphertext).array();
    }

    static byte[] open(String keyId, byte[] envelope, SecretKey key) throws GeneralSecurityException {
        byte[] aad = associatedData(keyId);
        if (envelope == null || envelope.length < HEADER.length + IV_BYTES + TAG_BITS / 8 + 1
                || envelope.length > MAX_ENVELOPE_BYTES) {
            throw new IllegalArgumentException("invalid-envelope");
        }
        for (int i = 0; i < HEADER.length; i++) {
            if (envelope[i] != HEADER[i]) throw new IllegalArgumentException("invalid-envelope");
        }
        byte[] iv = Arrays.copyOfRange(envelope, HEADER.length, HEADER.length + IV_BYTES);
        Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
        cipher.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(TAG_BITS, iv));
        cipher.updateAAD(aad);
        byte[] plaintext = cipher.doFinal(envelope, HEADER.length + IV_BYTES,
                envelope.length - HEADER.length - IV_BYTES);
        try {
            validateSecret(plaintext);
            return plaintext;
        } catch (RuntimeException failure) {
            Arrays.fill(plaintext, (byte) 0);
            throw failure;
        }
    }
}
