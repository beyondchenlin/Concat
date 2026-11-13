// SPDX-License-Identifier: AGPL-3.0-or-later
package app.concat.editor;

import android.content.Context;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.system.Os;
import android.system.OsConstants;
import java.io.File;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.RandomAccessFile;
import java.nio.channels.FileLock;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.NoSuchFileException;
import java.nio.file.attribute.BasicFileAttributes;
import java.security.Key;
import java.security.KeyStore;
import java.util.Arrays;
import javax.crypto.AEADBadTagException;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;

/**
 * Native settings only: never registered on a WebView or exposed to tools.
 * AndroidKeyStore protects one key per product-scoped identity; only GCM
 * ciphertext lives in the app's no-backup private directory.
 */
public final class BKCatCredentials {
    private static final int OK = 0;
    private static final int MISSING = 1;
    private static final int UNAVAILABLE = 2;
    private static final int CORRUPT = 3;
    private static final int INVALID = 4;

    private BKCatCredentials() {}

    /** First byte is a fixed result category. Secret bytes follow only OK. */
    public static synchronized byte[] read(Context context, String keyId) {
        if (!BKCatCredentialPolicy.validKeyId(keyId)) return result(INVALID);
        byte[] plaintext = null;
        try {
            File directory = directory(context);
            File stored = scopedFile(directory, keyId + ".bin");
            try (RandomAccessFile guard = new RandomAccessFile(scopedFile(directory, keyId + ".lock"), "rw");
                 FileLock lock = guard.getChannel().lock()) {
                if (!lock.isValid()) return result(UNAVAILABLE);
                BasicFileAttributes attributes;
                try {
                    attributes = Files.readAttributes(stored.toPath(), BasicFileAttributes.class,
                            LinkOption.NOFOLLOW_LINKS);
                } catch (NoSuchFileException absent) {
                    return result(MISSING);
                }
                if (!attributes.isRegularFile() || attributes.size() > BKCatCredentialPolicy.MAX_ENVELOPE_BYTES) {
                    return result(CORRUPT);
                }
                SecretKey key = key(keyId, false);
                if (key == null) return result(CORRUPT);
                plaintext = BKCatCredentialPolicy.open(keyId, Files.readAllBytes(stored.toPath()), key);
                byte[] answer = new byte[plaintext.length + 1];
                answer[0] = OK;
                System.arraycopy(plaintext, 0, answer, 1, plaintext.length);
                return answer;
            }
        } catch (AEADBadTagException | IllegalArgumentException failure) {
            return result(CORRUPT);
        } catch (Exception failure) {
            // Fixed categories only: provider exception text can contain
            // paths, aliases or request inputs and must not reach a log.
            return result(UNAVAILABLE);
        } finally {
            if (plaintext != null) Arrays.fill(plaintext, (byte) 0);
        }
    }

    public static synchronized int write(Context context, String keyId, byte[] secret) {
        try {
            if (!BKCatCredentialPolicy.validKeyId(keyId)) return INVALID;
            BKCatCredentialPolicy.validateSecret(secret);
            File directory = directory(context);
            File destination = scopedFile(directory, keyId + ".bin");
            try (RandomAccessFile guard = new RandomAccessFile(scopedFile(directory, keyId + ".lock"), "rw");
                 FileLock lock = guard.getChannel().lock()) {
                if (!lock.isValid()) return UNAVAILABLE;
                byte[] ciphertext = BKCatCredentialPolicy.seal(keyId, secret, key(keyId, true));
                File temporary = File.createTempFile(keyId + ".", ".encrypted-new", directory);
                try {
                    // No plaintext is ever passed to an output stream.
                    try (FileOutputStream output = new FileOutputStream(temporary)) {
                        output.write(ciphertext);
                        output.flush();
                        output.getFD().sync();
                    }
                    Os.rename(temporary.getAbsolutePath(), destination.getAbsolutePath());
                    syncDirectory(directory);
                } finally {
                    // Only the temporary file this operation just created.
                    if (temporary.exists() && !temporary.delete()) throw new IOException("temporary-cleanup-failed");
                }
                return OK;
            }
        } catch (IllegalArgumentException failure) {
            return INVALID;
        } catch (Exception failure) {
            return UNAVAILABLE;
        } finally {
            // JNI supplied a dedicated byte-array copy, not the Rust buffer.
            if (secret != null) Arrays.fill(secret, (byte) 0);
        }
    }

    public static synchronized int delete(Context context, String keyId) {
        if (!BKCatCredentialPolicy.validKeyId(keyId)) return INVALID;
        try {
            File directory = directory(context);
            File stored = scopedFile(directory, keyId + ".bin");
            try (RandomAccessFile guard = new RandomAccessFile(scopedFile(directory, keyId + ".lock"), "rw");
                 FileLock lock = guard.getChannel().lock()) {
                if (!lock.isValid()) return UNAVAILABLE;
                // Absent ciphertext is idempotent; access errors still throw.
                if (Files.deleteIfExists(stored.toPath())) syncDirectory(directory);
                KeyStore store = KeyStore.getInstance("AndroidKeyStore");
                store.load(null);
                String alias = BKCatCredentialPolicy.ALIAS_PREFIX + keyId;
                if (store.containsAlias(alias)) store.deleteEntry(alias);
                return OK;
            }
        } catch (Exception failure) {
            return UNAVAILABLE;
        }
    }

    private static byte[] result(int code) {
        return new byte[] { (byte) code };
    }

    private static File directory(Context context) throws IOException {
        File parent = context.getNoBackupFilesDir().getCanonicalFile();
        File directory = new File(parent, "bk-cat-credentials-v1");
        Files.createDirectories(directory.toPath());
        BasicFileAttributes attributes = Files.readAttributes(directory.toPath(),
                BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS);
        if (!attributes.isDirectory() || !directory.getCanonicalFile().equals(directory.getAbsoluteFile())) {
            throw new IOException("invalid-private-directory");
        }
        return directory;
    }

    private static File scopedFile(File directory, String name) throws IOException {
        File file = new File(directory, name);
        if (!file.getCanonicalFile().equals(file.getAbsoluteFile())) throw new IOException("invalid-private-file");
        return file;
    }

    private static SecretKey key(String keyId, boolean create) throws Exception {
        KeyStore store = KeyStore.getInstance("AndroidKeyStore");
        store.load(null);
        String alias = BKCatCredentialPolicy.ALIAS_PREFIX + keyId;
        if (store.containsAlias(alias)) {
            Key key = store.getKey(alias, null);
            if (!(key instanceof SecretKey)) throw new IOException("invalid-stored-key");
            return (SecretKey) key;
        }
        if (!create) return null;
        KeyGenerator generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(alias,
                KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .setRandomizedEncryptionRequired(true)
                .setUserAuthenticationRequired(false)
                .build());
        return generator.generateKey();
    }

    private static void syncDirectory(File directory) throws Exception {
        FileDescriptor descriptor = Os.open(directory.getAbsolutePath(),
                OsConstants.O_RDONLY, 0);
        try {
            Os.fsync(descriptor);
        } finally {
            Os.close(descriptor);
        }
    }
}
