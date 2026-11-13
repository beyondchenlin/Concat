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
        System.out.println("Android host policy: " + cases + " cases passed");
    }
}
