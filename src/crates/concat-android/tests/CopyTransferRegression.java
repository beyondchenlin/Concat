// SPDX-License-Identifier: AGPL-3.0-or-later
package app.concat.editor;

import java.io.ByteArrayInputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;

/** Host regression for the exact production copy and ownership implementations.
 * This is not an Android lifecycle, permission or device acceptance test. */
public final class CopyTransferRegression {
    private static int passed;

    private static void check(boolean value, String reason) {
        if (!value) throw new AssertionError(reason);
    }

    private static File file(Path root, String name) { return root.resolve(name).toFile(); }

    private static ConcatFiles.OwnedCopies owner(Path root, String name) throws IOException {
        ConcatFiles.OwnedCopies copies = new ConcatFiles.OwnedCopies();
        copies.directory(file(root, name));
        return copies;
    }

    private static void passed(String name) { passed++; System.out.println("PASS " + name); }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("one empty test output directory is required");
        Path root = new File(args[0]).toPath();
        Files.createDirectory(root);

        byte[] payload = new byte[160001];
        for (int i = 0; i < payload.length; i++) payload[i] = (byte) (i * 37);
        File complete = file(root, "complete.bin");
        List<Long> progress = new ArrayList<Long>();
        ConcatFiles.copyStream(new ByteArrayInputStream(payload), complete, progress::add);
        check(Arrays.equals(payload, Files.readAllBytes(complete.toPath())), "completed copy differs from input");
        long previous = -1;
        for (long value : progress) { check(value >= previous, "byte progress reversed"); previous = value; }
        check(previous == payload.length, "final byte count differs from closed file");
        passed("exact complete copy and actual byte progress");

        File before = file(root, "cancel-before.bin");
        AtomicBoolean read = new AtomicBoolean();
        try {
            ConcatFiles.copyStream(new InputStream() {
                @Override public int read() { read.set(true); return -1; }
            }, before, bytes -> { throw new ConcatFiles.CopyCancelled(); });
            throw new AssertionError("cancel before read returned success");
        } catch (ConcatFiles.CopyCancelled expected) {}
        check(!read.get() && !before.exists(), "early cancellation read or retained output");
        passed("cancel before first read removes reserved output");

        File partial = file(root, "cancel-partial.bin");
        try {
            ConcatFiles.copyStream(new ByteArrayInputStream(payload), partial, bytes -> {
                if (bytes > 0) throw new ConcatFiles.CopyCancelled();
            });
            throw new AssertionError("cancel after bytes returned success");
        } catch (ConcatFiles.CopyCancelled expected) {}
        check(!partial.exists(), "cancel left partial bytes");
        passed("cancel after a written chunk removes partial output");

        File blocked = file(root, "cancel-blocked.bin");
        CountDownLatch entered = new CountDownLatch(1), release = new CountDownLatch(1);
        AtomicBoolean cancelled = new AtomicBoolean();
        AtomicReference<Throwable> failure = new AtomicReference<Throwable>();
        Thread worker = new Thread(() -> {
            try {
                ConcatFiles.copyStream(new InputStream() {
                    @Override public int read() { throw new AssertionError("array reads expected"); }
                    @Override public int read(byte[] buffer) throws IOException {
                        entered.countDown();
                        try {
                            if (!release.await(5, TimeUnit.SECONDS)) throw new IOException("test read was not released");
                        } catch (InterruptedException error) { throw new IOException(error); }
                        Arrays.fill(buffer, (byte) 91);
                        return buffer.length;
                    }
                }, blocked, bytes -> { if (cancelled.get()) throw new ConcatFiles.CopyCancelled(); });
                failure.set(new AssertionError("blocked read was accepted after cancellation"));
            } catch (ConcatFiles.CopyCancelled expected) {
            } catch (Throwable error) { failure.set(error); }
        });
        worker.start();
        check(entered.await(5, TimeUnit.SECONDS), "copy did not reach provider read");
        cancelled.set(true);
        release.countDown();
        worker.join(5000);
        check(!worker.isAlive(), "cancelled copy did not finish");
        if (failure.get() != null) throw new AssertionError("blocked copy failed", failure.get());
        check(!blocked.exists(), "late provider bytes left an output");
        passed("cancellation during provider read prevents late write");

        File failed = file(root, "provider-failed.bin");
        try {
            ConcatFiles.copyStream(new InputStream() {
                int count;
                @Override public int read() { throw new AssertionError("array reads expected"); }
                @Override public int read(byte[] buffer) throws IOException {
                    if (count++ > 0) throw new IOException("controlled provider failure");
                    buffer[0] = 7; return 1;
                }
            }, failed, bytes -> {});
            throw new AssertionError("provider failure returned success");
        } catch (IOException expected) {
            check(expected.getMessage().equals("controlled provider failure"), "wrong provider error");
        }
        check(!failed.exists() && complete.exists(), "failure cleanup damaged unrelated copy");
        passed("provider failure removes only its partial output");

        ConcatFiles.OwnedCopies old = owner(root, "old");
        File oldFile = old.reserve("same.mp4");
        Files.write(oldFile.toPath(), payload);
        File foreign = file(root.resolve("old"), "foreign.txt");
        Files.write(foreign.toPath(), new byte[] {1});
        ConcatFiles.OwnedCopies newer = owner(root, "newer");
        File newerFile = newer.reserve("same.mp4");
        check(old.cancel() && !old.cancel(), "cancellation did not settle exactly once");
        check(!old.accept(), "retired selection was accepted");
        check(old.cleanup().equals(Arrays.asList(file(root, "old"))), "unknown file was not preserved");
        check(!oldFile.exists() && foreign.exists() && newerFile.exists(), "retired cleanup crossed ownership");
        old.cleanup();
        check(foreign.exists() && newerFile.exists(), "repeat cleanup crossed ownership");
        passed("retirement preserves new selection and unowned contents");

        ConcatFiles.OwnedCopies accepted = owner(root, "accepted");
        File first = accepted.reserve("duplicate.png"), second = accepted.reserve("duplicate.png");
        Files.write(first.toPath(), payload);
        Files.write(second.toPath(), new byte[] {2});
        check(first.getName().equals("duplicate.png") && second.getName().equals("duplicate (2).png"), "relative selection naming was reordered");
        check(accepted.accept() && !accepted.cancel(), "accepted files were recancelled");
        accepted.cleanup();
        check(first.exists() && second.exists(), "late cleanup removed accepted files");
        passed("accepted files survive late cancellation and retain order");

        ConcatFiles.OwnedCopies early = new ConcatFiles.OwnedCopies();
        early.cancel();
        try { early.directory(file(root, "too-late")); throw new AssertionError("created directory after cancellation"); }
        catch (ConcatFiles.CopyCancelled expected) {}
        check(!file(root, "too-late").exists(), "late selection directory exists");
        ConcatFiles.OwnedCopies queued = owner(root, "queued");
        File queuedFile = queued.reserve("finished.bin");
        ConcatFiles.copyStream(new ByteArrayInputStream(payload), queuedFile, bytes -> {});
        queued.cancel(); queued.cleanup();
        check(!queued.accept() && !queuedFile.exists(), "queued result escaped cancellation");
        passed("cancel before reserve and after complete before acceptance");

        for (int iteration = 0; iteration < 32; iteration++) {
            ConcatFiles.OwnedCopies racing = owner(root, "race-" + iteration);
            File racingFile = racing.reserve("media.bin");
            CyclicBarrier gate = new CyclicBarrier(2);
            AtomicBoolean kept = new AtomicBoolean();
            AtomicReference<Throwable> raceFailure = new AtomicReference<Throwable>();
            Thread accepting = new Thread(() -> {
                try { gate.await(5, TimeUnit.SECONDS); kept.set(racing.accept()); }
                catch (Throwable error) { raceFailure.set(error); }
            });
            accepting.start();
            gate.await(5, TimeUnit.SECONDS);
            boolean stopped = racing.cancel();
            accepting.join(5000);
            check(!accepting.isAlive() && raceFailure.get() == null, "accept/cancel race did not settle");
            check(stopped != kept.get(), "acceptance and cancellation chose inconsistent owners");
            racing.cleanup();
            check(racingFile.exists() == kept.get(), "cleanup disagrees with terminal ownership");
        }
        passed("concurrent acceptance and cancellation have one owner");

        ConcatFiles.OwnedCopies empty = owner(root, "empty-failure");
        check(empty.accept() && !file(root, "empty-failure").exists(), "empty failed selection left a directory");
        passed("empty failed selection leaves no empty directory");
        System.out.println("RESULT " + passed + " passed");
    }
}
