// SPDX-License-Identifier: AGPL-3.0-or-later
package app.concat.editor;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;

/** Host policy checks. These do not claim to launch an Android WebView. */
public final class BKCatRuntimePolicyTest {
    private static int cases;
    private static void check(boolean condition, String detail) {
        if (!condition) throw new AssertionError(detail);
    }
    private static void pass(String name) {
        cases++;
        System.out.println("PASS " + name);
    }
    public static void main(String[] args) throws Exception {
        for (String path : new String[] {
                "/index.html", "/runtime.js", "/bootstrap.js", "/transport.js"}) {
            check(path.equals(BKCatRuntimePolicy.resourcePath(
                    BKCatRuntimePolicy.ORIGIN + path, "GET")), "bundled asset");
        }
        pass("four bundled resources are admitted");
        String[] denied = {
            "http://bk-cat.invalid/index.html", "https://bk-cat.invalid:443/index.html",
            "https://bk-cat.invalid.evil.test/index.html", "https://user@bk-cat.invalid/index.html",
            "https://bk-cat.invalid/index.html?script=external", "https://bk-cat.invalid/index.html#fragment",
            "https://bk-cat.invalid/%69ndex.html", "https://bk-cat.invalid/./index.html",
            "https://bk-cat.invalid/../index.html", "https://bk-cat.invalid//index.html",
            "https://bk-cat.invalid/secret.sqlite", "https://bk-cat.invalid/",
            "file:///data/data/app.concat.editor/secret", "content://media/external/video/1",
            "javascript:alert(1)", "data:text/html,secret", "about:blank",
            "https://BK-CAT.invalid/index.html", null
        };
        for (String url : denied) {
            check(BKCatRuntimePolicy.resourcePath(url, "GET") == null, "foreign resource " + url);
        }
        for (String method : new String[] {"POST", "PUT", "DELETE", "HEAD", null}) {
            check(BKCatRuntimePolicy.resourcePath(BKCatRuntimePolicy.DOCUMENT, method) == null,
                    "non-read request " + method);
        }
        pass("foreign origins, hidden paths and mutation methods are denied");
        BKCatRuntimePolicy policy = new BKCatRuntimePolicy();
        check(!policy.canMessage(BKCatRuntimePolicy.DOCUMENT, "{}"), "no bridge before document");
        check(policy.beginDocument(BKCatRuntimePolicy.DOCUMENT), "one initial native document");
        check(policy.canMessage(BKCatRuntimePolicy.DOCUMENT, "{}"), "trusted document may send");
        check(!policy.canMessage("https://evil.test", "{}"), "foreign document cannot send");
        check(!policy.canMessage(BKCatRuntimePolicy.DOCUMENT, null), "null cannot send");
        check(!policy.canMessage(BKCatRuntimePolicy.DOCUMENT,
                new String(new char[BKCatRuntimePolicy.MAX_MESSAGE_CHARS + 1])), "oversize cannot send");
        char[] wide = new char[BKCatRuntimePolicy.MAX_MESSAGE_CHARS / 2];
        java.util.Arrays.fill(wide, '\u4e2d');
        check(!policy.canMessage(BKCatRuntimePolicy.DOCUMENT, new String(wide)),
                "multibyte text is bounded by native UTF-8 bytes");
        pass("messages require the trusted document and bounded data");
        check(!policy.beginDocument(BKCatRuntimePolicy.DOCUMENT), "reload changes document lifetime");
        check(!policy.canMessage(BKCatRuntimePolicy.DOCUMENT, "{}"), "reload retires authority");
        check(!policy.beginDocument(BKCatRuntimePolicy.DOCUMENT), "reload cannot revive");
        pass("same-origin reload permanently retires the old component");
        BKCatRuntimePolicy beforeCreate = new BKCatRuntimePolicy();
        check(beforeCreate.retire(), "initial retirement wins");
        check(!beforeCreate.retire(), "retirement is idempotent");
        check(!beforeCreate.beginDocument(BKCatRuntimePolicy.DOCUMENT), "late start after pause fails");
        check(!beforeCreate.canMessage(BKCatRuntimePolicy.DOCUMENT, "{}"), "late callback fails");
        pass("pause before creation cannot resurrect a component");
        BKCatRuntimePolicy old = new BKCatRuntimePolicy();
        check(old.beginDocument(BKCatRuntimePolicy.DOCUMENT), "old document starts");
        CountDownLatch queued = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        AtomicBoolean delivered = new AtomicBoolean(false);
        Thread delayed = new Thread(() -> {
            queued.countDown();
            try { release.await(); } catch (InterruptedException interrupted) {
                Thread.currentThread().interrupt();
                throw new AssertionError(interrupted);
            }
            delivered.set(old.canMessage(BKCatRuntimePolicy.DOCUMENT, "{\"late\":true}"));
        });
        delayed.start();
        queued.await();
        old.retire();
        BKCatRuntimePolicy replacement = new BKCatRuntimePolicy();
        check(replacement.beginDocument(BKCatRuntimePolicy.DOCUMENT), "fresh policy starts fresh");
        release.countDown();
        delayed.join();
        check(!delivered.get(), "queued old callback is refused after native pause");
        check(replacement.canMessage(BKCatRuntimePolicy.DOCUMENT, "{}"), "new host remains independent");
        pass("a delayed callback cannot cross a native component handoff");
        BKCatRuntimePolicy.InitialCreation pending =
                new BKCatRuntimePolicy.InitialCreation(100L);
        check(pending.deadlineMillis() == 10_100L, "fixed absolute initial deadline");
        check(pending.poll(false, 100L) == BKCatRuntimePolicy.InitialCreation.Decision.WAIT,
                "missing first focus waits without opening");
        check(pending.poll(false, 9_999L) == BKCatRuntimePolicy.InitialCreation.Decision.WAIT,
                "waiting does not create a document");
        check(pending.poll(true, 10_099L) == BKCatRuntimePolicy.InitialCreation.Decision.OPEN,
                "first foreground focus before deadline opens once");
        check(pending.poll(true, 10_100L) == BKCatRuntimePolicy.InitialCreation.Decision.OPENED,
                "a later callback cannot create a second document");
        pass("initial focus waits and admits exactly one document before its fixed deadline");

        BKCatRuntimePolicy.InitialCreation foreground =
                new BKCatRuntimePolicy.InitialCreation(50L);
        check(foreground.poll(true, 50L) == BKCatRuntimePolicy.InitialCreation.Decision.OPEN,
                "an already foreground window does not wait");
        check(foreground.poll(false, 51L) == BKCatRuntimePolicy.InitialCreation.Decision.OPENED,
                "focus notifications do not recreate an opened component");
        pass("an initially focused window opens immediately only once");

        for (boolean focusedAtDeadline : new boolean[] {false, true}) {
            BKCatRuntimePolicy.InitialCreation expired =
                    new BKCatRuntimePolicy.InitialCreation(200L);
            check(expired.poll(focusedAtDeadline, 10_200L)
                            == BKCatRuntimePolicy.InitialCreation.Decision.EXPIRED,
                    "the absolute deadline wins even if focus is now true");
            check(expired.poll(true, 10_201L)
                            == BKCatRuntimePolicy.InitialCreation.Decision.RETIRED,
                    "focus after timeout cannot revive the old request");
        }
        pass("a first focus at or after the deadline is refused permanently");

        // These are the policy inputs used by lifecycle and native-close
        // handlers, not an assertion that Android callbacks ran in this JVM.
        for (String retirement : new String[] {"pause", "save", "destroy", "native-close"}) {
            BKCatRuntimePolicy.InitialCreation cancelled =
                    new BKCatRuntimePolicy.InitialCreation(0L);
            check(cancelled.poll(false, 1L)
                            == BKCatRuntimePolicy.InitialCreation.Decision.WAIT,
                    "pending before " + retirement);
            cancelled.retire();
            check(cancelled.poll(true, 2L)
                            == BKCatRuntimePolicy.InitialCreation.Decision.RETIRED,
                    "foreground after " + retirement + " cannot reopen");
            cancelled.retire();
            check(cancelled.poll(false, 3L)
                            == BKCatRuntimePolicy.InitialCreation.Decision.RETIRED,
                    "retirement stays terminal");
        }
        pass("retirement during the initial wait cannot be undone by resumed focus");

        BKCatRuntimePolicy.InitialCreation queuedFocus =
                new BKCatRuntimePolicy.InitialCreation(0L);
        CountDownLatch focusQueued = new CountDownLatch(1);
        CountDownLatch deliverFocus = new CountDownLatch(1);
        AtomicBoolean openedAfterStop = new AtomicBoolean(false);
        Thread lateFocus = new Thread(() -> {
            focusQueued.countDown();
            try { deliverFocus.await(); } catch (InterruptedException interrupted) {
                Thread.currentThread().interrupt();
                throw new AssertionError(interrupted);
            }
            openedAfterStop.set(queuedFocus.poll(true, 1L)
                    == BKCatRuntimePolicy.InitialCreation.Decision.OPEN);
        });
        lateFocus.start();
        focusQueued.await();
        queuedFocus.retire();
        deliverFocus.countDown();
        lateFocus.join();
        check(!openedAfterStop.get(), "a queued focus notification loses to completed retirement");
        pass("a delayed first-focus callback cannot cross a completed stop");

        foreground.retire();
        check(foreground.poll(true, 100L) == BKCatRuntimePolicy.InitialCreation.Decision.RETIRED,
                "an opened and retired component stays retired");
        BKCatRuntimePolicy.InitialCreation explicitReplacement =
                new BKCatRuntimePolicy.InitialCreation(100L);
        check(explicitReplacement.poll(true, 100L)
                        == BKCatRuntimePolicy.InitialCreation.Decision.OPEN,
                "only a separately constructed component receives new startup state");
        check(queuedFocus.poll(true, 100L)
                        == BKCatRuntimePolicy.InitialCreation.Decision.RETIRED,
                "a replacement cannot reset the previous request");
        pass("a fresh explicit component is independent of every retired request");

        System.out.println("Android host policy: " + cases + " cases passed");
    }
}
