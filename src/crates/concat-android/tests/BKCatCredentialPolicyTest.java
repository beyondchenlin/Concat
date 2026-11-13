// SPDX-License-Identifier: AGPL-3.0-or-later
package app.concat.editor;

import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.SecureRandom;
import java.util.Arrays;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;

/** Real envelope crypto under the JDK. This is NOT AndroidKeyStore execution. */
public final class BKCatCredentialPolicyTest {
    private interface Checked { void run() throws Exception; }

    private static void require(boolean condition, String name) {
        if (!condition) throw new AssertionError(name);
    }

    private static void refused(Checked body, String name) throws Exception {
        try {
            body.run();
        } catch (GeneralSecurityException | IllegalArgumentException expected) {
            return;
        }
        throw new AssertionError(name);
    }

    private static String hex(byte[] bytes) {
        StringBuilder result = new StringBuilder();
        for (byte value : bytes) result.append(String.format("%02x", value & 255));
        return result.toString();
    }

    public static void main(String[] args) throws Exception {
        SecureRandom random = new SecureRandom();
        byte[] identity = new byte[32];
        byte[] generated = new byte[32];
        random.nextBytes(identity);
        random.nextBytes(generated);
        String keyId = hex(identity);
        byte[] secret = hex(generated).getBytes(StandardCharsets.US_ASCII);
        KeyGenerator generator = KeyGenerator.getInstance("AES");
        generator.init(256, random);
        SecretKey testKey = generator.generateKey();
        try {
            require(BKCatCredentialPolicy.validKeyId(keyId), "generated scope accepted");
            for (String invalid : new String[] { "", "../other", keyId.toUpperCase(), keyId + "0", null }) {
                require(!BKCatCredentialPolicy.validKeyId(invalid), "invalid scope refused");
            }
            System.out.println("PASS: only fixed-format product identity accepted");

            byte[] first = BKCatCredentialPolicy.seal(keyId, secret, testKey);
            byte[] recovered = BKCatCredentialPolicy.open(keyId, first, testKey);
            require(Arrays.equals(recovered, secret), "authenticated roundtrip");
            Arrays.fill(recovered, (byte) 0);
            require(!new String(first, StandardCharsets.ISO_8859_1).contains(
                    new String(secret, StandardCharsets.US_ASCII)), "ciphertext does not contain this plaintext");
            System.out.println("PASS: actual authenticated encryption roundtrip");

            byte[] second = BKCatCredentialPolicy.seal(keyId, secret, testKey);
            require(!Arrays.equals(first, second), "fresh randomized IV");
            System.out.println("PASS: repeated writes use different randomized ciphertext");

            String other = (keyId.charAt(0) == '0' ? "1" : "0") + keyId.substring(1);
            refused(() -> BKCatCredentialPolicy.open(other, first, testKey), "scope substitution");
            System.out.println("PASS: copied ciphertext cannot authenticate another destination");

            byte[] changed = first.clone();
            changed[changed.length - 1] ^= 1;
            refused(() -> BKCatCredentialPolicy.open(keyId, changed, testKey), "ciphertext tamper");
            System.out.println("PASS: modified ciphertext is rejected");

            byte[] wrongVersion = first.clone();
            wrongVersion[4] ^= 1;
            refused(() -> BKCatCredentialPolicy.open(keyId, wrongVersion, testKey), "unknown envelope");
            refused(() -> BKCatCredentialPolicy.open(keyId, Arrays.copyOf(first, 10), testKey), "truncated envelope");
            refused(() -> BKCatCredentialPolicy.open(keyId,
                    new byte[BKCatCredentialPolicy.MAX_ENVELOPE_BYTES + 1], testKey), "oversized envelope");
            System.out.println("PASS: version, truncation and length failures are explicit");

            refused(() -> BKCatCredentialPolicy.seal(keyId, new byte[0], testKey), "empty secret");
            refused(() -> BKCatCredentialPolicy.seal(keyId,
                    new byte[BKCatCredentialPolicy.MAX_SECRET_BYTES + 1], testKey), "oversized secret");
            refused(() -> BKCatCredentialPolicy.seal(keyId, new byte[] { 'a', '\n' }, testKey), "control bytes");
            System.out.println("PASS: invalid secret bounds fail before persistence");
        } finally {
            Arrays.fill(secret, (byte) 0);
            Arrays.fill(generated, (byte) 0);
            Arrays.fill(identity, (byte) 0);
        }
        System.out.println("NOT RUN: AndroidKeyStore, private-file I/O, JNI or device execution");
    }
}
