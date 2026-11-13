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
        BKCatRuntimePolicy initial = new BKCatRuntimePolicy();
        check(initial.startView(false, 100, 200) == BKCatRuntimePolicy.StartupDecision.WAITING,
                "initial request waits for the first window focus");
        check(initial.active(), "waiting does not retire the initial request");
        check(initial.startView(false, 199, 200) == BKCatRuntimePolicy.StartupDecision.WAITING,
                "repeated unfocused callbacks do not create a view");
        check(initial.startView(true, 199, 200) == BKCatRuntimePolicy.StartupDecision.START,
                "first focus before the deadline admits exactly one view");
        check(initial.startView(true, 199, 200) == BKCatRuntimePolicy.StartupDecision.ALREADY_STARTED,
                "duplicate focus cannot create another view");
        check(initial.startView(false, 201, 200) == BKCatRuntimePolicy.StartupDecision.ALREADY_STARTED
                && initial.active(), "a stale timeout cannot retire an already-started view");
        pass("initial focus waits, starts once and rejects duplicate creation");

        for (String cause : new String[] {"pause", "stop", "saved-state", "destroy", "native-close"}) {
            BKCatRuntimePolicy waiting = new BKCatRuntimePolicy();
            check(waiting.startView(false, 100, 200) == BKCatRuntimePolicy.StartupDecision.WAITING,
                    cause + " begins while initial focus is pending");
            waiting.retire();
            check(waiting.startView(true, 150, 200) == BKCatRuntimePolicy.StartupDecision.RETIRED,
                    cause + " permanently rejects late focus");
            check(!waiting.beginDocument(BKCatRuntimePolicy.DOCUMENT)
                    && !waiting.canMessage(BKCatRuntimePolicy.DOCUMENT, "{}"),
                    cause + " rejects late document and message callbacks");
        }
        pass("pause, stop, saved-state, destruction and close before focus cannot revive");

        BKCatRuntimePolicy expired = new BKCatRuntimePolicy();
        check(expired.startView(false, 200, 200) == BKCatRuntimePolicy.StartupDecision.TIMED_OUT,
                "the exact initial deadline fails closed");
        check(!expired.active(), "timeout retires authority");
        check(expired.startView(true, 201, 200) == BKCatRuntimePolicy.StartupDecision.RETIRED,
                "focus after timeout cannot revive");
        check(!expired.beginDocument(BKCatRuntimePolicy.DOCUMENT), "timeout refuses a late document");
        BKCatRuntimePolicy lateFocus = new BKCatRuntimePolicy();
        check(lateFocus.startView(true, 201, 200) == BKCatRuntimePolicy.StartupDecision.TIMED_OUT,
                "late focus cannot beat a delayed timeout callback");
        pass("initial deadline rejects absent and late focus without extending itself");

        BKCatRuntimePolicy retiredView = new BKCatRuntimePolicy();
        check(retiredView.startView(true, 100, 200) == BKCatRuntimePolicy.StartupDecision.START,
                "ready initial request starts");
        retiredView.retire();
        check(retiredView.startView(true, 150, 200) == BKCatRuntimePolicy.StartupDecision.RETIRED,
                "resumed focus never restarts a previously opened and retired view");
        pass("retired opened component is never restarted by focus recovery");

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
        System.out.println("Android host policy: " + cases + " cases passed");
    }
}
