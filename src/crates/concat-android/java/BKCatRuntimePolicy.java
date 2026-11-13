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

    /**
     * One initial, never-opened component may await foreground focus for a
     * bounded interval. A retired component has no path back to WAIT or OPEN.
     * The production UI handler supplies Android's monotonic uptime clock.
     */
    public static final class InitialCreation {
        public static final long TIMEOUT_MILLIS = 10_000L;
        public enum Decision { WAIT, OPEN, OPENED, EXPIRED, RETIRED }

        private final long deadlineMillis;
        private boolean opened;
        private boolean retired;

        public InitialCreation(long startedAtMillis) {
            if (startedAtMillis < 0
                    || startedAtMillis > Long.MAX_VALUE - TIMEOUT_MILLIS) {
                throw new IllegalArgumentException("Invalid monotonic start time");
            }
            deadlineMillis = startedAtMillis + TIMEOUT_MILLIS;
        }

        public long deadlineMillis() { return deadlineMillis; }

        public synchronized Decision poll(boolean foreground, long nowMillis) {
            if (retired) return Decision.RETIRED;
            if (opened) return Decision.OPENED;
            if (nowMillis >= deadlineMillis) {
                retired = true;
                return Decision.EXPIRED;
            }
            if (!foreground) return Decision.WAIT;
            opened = true;
            return Decision.OPEN;
        }

        public synchronized void retire() { retired = true; }
    }
}
