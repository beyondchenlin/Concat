// SPDX-License-Identifier: AGPL-3.0-or-later

package app.concat.editor;

/**
 * The admissible document and resource set of one native runtime component.
 * This policy has no Android dependency so navigation and retirement races
 * are tested without pretending to have run an Android WebView.
 */
public final class BKCatRuntimePolicy {
    public static final String ORIGIN = "https://bk-cat.invalid";
    public static final String DOCUMENT = ORIGIN + "/index.html";
    public static final int MAX_MESSAGE_CHARS = 4 * 1024 * 1024;

    private boolean active = true;
    private boolean documentStarted = false;
    private boolean viewStarted = false;

    public enum StartupDecision { WAITING, START, ALREADY_STARTED, RETIRED, TIMED_OUT }

    /**
     * A single initial creation may wait for focus. The caller supplies Android's
     * monotonic uptime and one fixed deadline; later focus cannot revive a
     * retired component, and a stale timeout cannot retire an already-open view.
     */
    public synchronized StartupDecision startView(boolean foregroundReady,
            long now, long deadline) {
        if (!active) return StartupDecision.RETIRED;
        if (viewStarted) return StartupDecision.ALREADY_STARTED;
        if (now >= deadline) {
            active = false;
            return StartupDecision.TIMED_OUT;
        }
        if (!foregroundReady) return StartupDecision.WAITING;
        viewStarted = true;
        return StartupDecision.START;
    }

    /** No redirects, credentials, ports, queries, fragments or alternate schemes. */
    public static String resourcePath(String url, String method) {
        if (!"GET".equals(method)) return null;
        for (String path : new String[] {
                "/index.html", "/runtime.js", "/bootstrap.js", "/transport.js"}) {
            if ((ORIGIN + path).equals(url)) return path;
        }
        return null;
    }

    /** A component has exactly one document lifetime, including same-origin reloads. */
    public synchronized boolean beginDocument(String url) {
        if (!active || documentStarted || !DOCUMENT.equals(url)) {
            active = false;
            return false;
        }
        documentStarted = true;
        return true;
    }

    /** Rechecked on the Android UI thread just before entering native code. */
    public synchronized boolean canMessage(String url, String message) {
        return active && documentStarted && DOCUMENT.equals(url)
                && message != null && message.length() <= MAX_MESSAGE_CHARS
                && message.getBytes(java.nio.charset.StandardCharsets.UTF_8).length
                        <= MAX_MESSAGE_CHARS;
    }

    public synchronized boolean active() {
        return active;
    }

    /** Returns true once, before any native close callback or WebView cleanup. */
    public synchronized boolean retire() {
        boolean previous = active;
        active = false;
        return previous;
    }
}
