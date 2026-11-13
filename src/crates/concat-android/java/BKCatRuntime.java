// SPDX-License-Identifier: AGPL-3.0-or-later

package app.concat.editor;

import android.annotation.SuppressLint;
import android.app.Activity;
import android.app.Application;
import android.graphics.Bitmap;
import android.net.http.SslError;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.util.Log;
import android.view.View;
import android.view.ViewGroup;
import android.view.ViewTreeObserver;
import android.webkit.ConsoleMessage;
import android.webkit.GeolocationPermissions;
import android.webkit.JavascriptInterface;
import android.webkit.PermissionRequest;
import android.webkit.RenderProcessGoneDetail;
import android.webkit.SslErrorHandler;
import android.webkit.ValueCallback;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;

import org.json.JSONObject;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.util.Collections;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * A script-only WebView beside Slint's existing NativeActivity surface.
 *
 * It loads only four immutable native resources. It never loads file/content
 * URLs, opens a browser, picks files, asks for web permissions or calls the
 * network. Model and database access must use the separately authorized native
 * bridge. Pausing, navigation and renderer loss permanently retire this
 * component; resuming the Activity never restarts it.
 */
@SuppressWarnings("deprecation")
public final class BKCatRuntime {
    private static final Handler UI = new Handler(Looper.getMainLooper());
    // Accessed only on Android's UI thread, independently of Slint's thread.
    private static final Map<String, BKCatRuntime> INSTANCES = new HashMap<>();
    private static final String BRIDGE = "BKCatAndroid";
    private static Lifecycle lifecycle;

    private final BKCatRuntimePolicy policy = new BKCatRuntimePolicy();
    private final AtomicBoolean disposed = new AtomicBoolean(false);
    private final Activity activity;
    private final String instance;
    private final Map<String, byte[]> resources;
    private WebView webView;
    private final BKCatRuntimePolicy.InitialCreation initialCreation =
            new BKCatRuntimePolicy.InitialCreation(SystemClock.uptimeMillis());
    private final Runnable initialDeadline = this::tryInitialOpen;
    private final ViewTreeObserver.OnWindowFocusChangeListener initialFocus = focused -> {
        if (focused) tryInitialOpen();
    };
    private View initialFocusView;

    /** Only fixed codes are logged; no instance capability, message or path. */
    private enum StopReason {
        ACTIVITY_FINISHING("activity-finishing", "Android activity is finishing"),
        ACTIVITY_DESTROYED("activity-destroyed", "Android activity was destroyed"),
        ACTIVITY_REPLACED("activity-replaced", "Android activity was replaced"),
        ACTIVITY_PAUSED("activity-paused", "Android activity paused; explicit recovery is required"),
        ACTIVITY_STOPPED("activity-stopped", "Android activity stopped"),
        ACTIVITY_SAVED("activity-saved", "Android activity state saved; explicit recovery is required"),
        OBSERVER_UNAVAILABLE("observer-unavailable", "Android lifecycle observer is unavailable"),
        PREVIOUS_COMPONENT_OPEN("previous-component-open", "The preceding Android runtime has not closed"),
        INITIAL_FOCUS_TIMEOUT("initial-focus-timeout", "Android window did not become foreground-ready"),
        INITIAL_FOCUS_OBSERVER_FAILED("initial-focus-observer-failed", "Android window focus could not be observed"),
        INITIAL_FOCUS_QUEUE_REJECTED("initial-focus-queue-rejected", "Android UI rejected the initial focus deadline"),
        UI_CREATE_REJECTED("ui-create-rejected", "Android UI rejected runtime creation"),
        WEBVIEW_CREATE_FAILED("webview-create-failed", "Android WebView creation failed"),
        COMPONENT_RETIRED("component-retired", "Android runtime has no active activity"),
        NATIVE_DELIVERY_FAILED("native-delivery-failed", "Android runtime rejected native delivery"),
        NATIVE_CLOSED("native-closed", "Native host closed"),
        UI_CLOSE_REJECTED("ui-close-rejected", "Android UI could not acknowledge destruction"),
        DOWNLOAD_FORBIDDEN("download-forbidden", "A runtime document attempted a download"),
        MESSAGE_TOO_LARGE("message-too-large", "Android runtime exceeded its message bound"),
        UI_MESSAGE_REJECTED("ui-message-rejected", "Android UI refused a runtime message"),
        NAVIGATION_FORBIDDEN("navigation-forbidden", "Runtime navigation is forbidden"),
        DOCUMENT_CHANGED("document-changed", "Runtime document identity changed"),
        HISTORY_CHANGED("history-changed", "Runtime document history changed"),
        TLS_FORBIDDEN("tls-forbidden", "Runtime TLS request is forbidden"),
        DOCUMENT_LOAD_FAILED("document-load-failed", "Runtime document failed to load"),
        RENDERER_EXITED("renderer-exited", "Android runtime renderer exited; native recovery is required");

        final String code;
        final String description;
        StopReason(String code, String description) {
            this.code = code;
            this.description = description;
        }
    }

    private static void reportStopping(String instance, StopReason reason) {
        Log.i("concat", "bk-cat-android-host: state=stopping reason=" + reason.code);
        runtimeStopping(instance, reason.description);
    }

    private static void reportClosed(String instance, StopReason reason) {
        Log.i("concat", "bk-cat-android-host: state=closed reason=" + reason.code);
        runtimeClosed(instance, reason.description);
    }

    private static StopReason activityProblem(Activity activity) {
        if (activity.isFinishing()) return StopReason.ACTIVITY_FINISHING;
        if (activity.isDestroyed()) return StopReason.ACTIVITY_DESTROYED;
        if (lifecycle == null) return StopReason.OBSERVER_UNAVAILABLE;
        if (lifecycle.activity != activity) return StopReason.ACTIVITY_REPLACED;
        if (lifecycle.paused) return StopReason.ACTIVITY_PAUSED;
        return null;
    }

    private BKCatRuntime(Activity activity, String instance, Map<String, byte[]> resources) {
        this.activity = activity;
        this.instance = instance;
        this.resources = resources;
    }

    /** JNI registers these before any document is loaded. */
    public static native void runtimeMessage(String instance, String message);
    public static native void runtimeStopping(String instance, String reason);
    public static native void runtimeClosed(String instance, String reason);

    /** Install before the native platform factory can create any document. */
    public static void install(final Activity activity) {
        if (!UI.post(() -> {
            if (activity.isFinishing() || activity.isDestroyed()) return;
            if (lifecycle != null && lifecycle.activity == activity) return;
            if (lifecycle != null) {
                lifecycle.retire(StopReason.ACTIVITY_REPLACED);
                lifecycle.activity.getApplication().unregisterActivityLifecycleCallbacks(lifecycle);
                lifecycle = null;
            }
            Lifecycle observer = new Lifecycle(activity);
            try {
                activity.getApplication().registerActivityLifecycleCallbacks(observer);
                lifecycle = observer;
            } catch (RuntimeException error) {
                // The factory's create request will refuse a missing observer.
            }
        })) throw new IllegalStateException("Android UI is unavailable");
    }

    /** Called on Slint's thread. No Android-UI/Slint synchronous wait. */
    public static void create(final Activity activity, final String instance,
            final String document, final String runtime, final String bootstrap,
            final String transport) {
        if (!UI.post(() -> {
            StopReason problem = activityProblem(activity);
            if (problem != null) {
                reportClosed(instance, problem);
                return;
            }
            if (!INSTANCES.isEmpty()) {
                reportClosed(instance, StopReason.PREVIOUS_COMPONENT_OPEN);
                return;
            }
            Map<String, byte[]> assets = new HashMap<>();
            assets.put("/index.html", document.getBytes(StandardCharsets.UTF_8));
            assets.put("/runtime.js", runtime.getBytes(StandardCharsets.UTF_8));
            assets.put("/bootstrap.js", bootstrap.getBytes(StandardCharsets.UTF_8));
            assets.put("/transport.js", transport.getBytes(StandardCharsets.UTF_8));
            BKCatRuntime host = new BKCatRuntime(activity, instance, Collections.unmodifiableMap(assets));
            INSTANCES.put(instance, host);
            host.tryInitialOpen();
        })) reportClosed(instance, StopReason.UI_CREATE_REJECTED);
    }

    /**
     * Continue only this never-opened component while its first window gains
     * focus. Pause/save/destroy/close retire it and cancel both observers.
     * Resuming an activity does not call this method or create a replacement.
     */
    private void tryInitialOpen() {
        if (disposed.get() || !policy.active()) return;
        StopReason problem = activityProblem(activity);
        if (problem != null) {
            dispose(problem);
            return;
        }
        BKCatRuntimePolicy.InitialCreation.Decision decision = initialCreation.poll(
                activity.hasWindowFocus(), SystemClock.uptimeMillis());
        switch (decision) {
            case WAIT:
                if (initialFocusView != null) return;
                try {
                    View decor = activity.getWindow().getDecorView();
                    ViewTreeObserver observer = decor.getViewTreeObserver();
                    if (!observer.isAlive()) {
                        dispose(StopReason.INITIAL_FOCUS_OBSERVER_FAILED);
                        return;
                    }
                    initialFocusView = decor;
                    observer.addOnWindowFocusChangeListener(initialFocus);
                    if (!UI.postAtTime(initialDeadline, initialCreation.deadlineMillis())) {
                        dispose(StopReason.INITIAL_FOCUS_QUEUE_REJECTED);
                        return;
                    }
                    Log.i("concat", "bk-cat-android-host: state=waiting reason=initial-window-focus");
                } catch (RuntimeException error) {
                    dispose(StopReason.INITIAL_FOCUS_OBSERVER_FAILED);
                }
                return;
            case OPEN:
                clearInitialWait();
                try {
                    openView();
                } catch (RuntimeException error) {
                    dispose(StopReason.WEBVIEW_CREATE_FAILED);
                }
                return;
            case EXPIRED:
                dispose(StopReason.INITIAL_FOCUS_TIMEOUT);
                return;
            case OPENED:
            case RETIRED:
                return;
            default:
                throw new AssertionError("Unhandled initial creation decision");
        }
    }

    private void clearInitialWait() {
        UI.removeCallbacks(initialDeadline);
        View decor = initialFocusView;
        initialFocusView = null;
        if (decor == null) return;
        try {
            // A pre-attach observer is merged into a new one when the decor
            // attaches. Always remove from the decor's current observer.
            ViewTreeObserver observer = decor.getViewTreeObserver();
            if (observer.isAlive()) observer.removeOnWindowFocusChangeListener(initialFocus);
        } catch (RuntimeException error) {
            // A retained callback cannot reopen a retired or already-opened
            // component; this does not acknowledge any WebView destruction.
            Log.w("concat", "bk-cat-android-host: state=cleanup reason=focus-listener-removal-failed");
        }
    }

    /** The string is data, never interpolated as source code. */
    public static void send(final String instance, final String message) {
        if (!UI.post(() -> {
            BKCatRuntime fragment = INSTANCES.get(instance);
            if (fragment == null || fragment.webView == null) return;
            if (!fragment.policy.canMessage(fragment.webView.getUrl(), message)) return;
            // JSON string quoting also escapes line separators for every JS engine.
            String quoted = JSONObject.quote(message)
                    .replace("\u2028", "\\u2028").replace("\u2029", "\\u2029");
            try {
                fragment.webView.evaluateJavascript(
                        "window.__BKCatDeliver(" + quoted + ");", null);
            } catch (RuntimeException error) {
                fragment.dispose(StopReason.NATIVE_DELIVERY_FAILED);
            }
        })) throw new IllegalStateException("Android UI rejected native delivery");
    }

    public static void close(final String instance) {
        if (!UI.post(() -> {
            BKCatRuntime fragment = INSTANCES.get(instance);
            if (fragment != null) fragment.dispose(StopReason.NATIVE_CLOSED);
        })) reportStopping(instance, StopReason.UI_CLOSE_REJECTED);
    }

    @SuppressLint("SetJavaScriptEnabled")
    private void openView() {
        if (!policy.active()) {
            dispose(StopReason.COMPONENT_RETIRED);
            return;
        }
        WebView.setWebContentsDebuggingEnabled(false);
        WebView view = new WebView(activity);
        webView = view;
        WebSettings settings = view.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(false);
        settings.setDatabaseEnabled(false);
        settings.setAllowFileAccess(false);
        settings.setAllowContentAccess(false);
        settings.setAllowFileAccessFromFileURLs(false);
        settings.setAllowUniversalAccessFromFileURLs(false);
        settings.setBlockNetworkLoads(true);
        settings.setBlockNetworkImage(true);
        settings.setLoadsImagesAutomatically(false);
        settings.setJavaScriptCanOpenWindowsAutomatically(false);
        settings.setSupportMultipleWindows(false);
        settings.setGeolocationEnabled(false);
        settings.setMediaPlaybackRequiresUserGesture(true);
        settings.setMixedContentMode(WebSettings.MIXED_CONTENT_NEVER_ALLOW);
        settings.setCacheMode(WebSettings.LOAD_NO_CACHE);
        view.setWebViewClient(new ControlledClient());
        view.setWebChromeClient(new WebChromeClient() {
            @Override public void onPermissionRequest(PermissionRequest request) {
                request.deny();
            }
            @Override public void onGeolocationPermissionsShowPrompt(
                    String origin, GeolocationPermissions.Callback callback) {
                callback.invoke(origin, false, false);
            }
            @Override public boolean onShowFileChooser(WebView webView,
                    ValueCallback<android.net.Uri[]> callback, FileChooserParams params) {
                callback.onReceiveValue(null);
                return true;
            }
            @Override public boolean onConsoleMessage(ConsoleMessage message) {
                // Do not log conversation, credentials or database contents.
                return true;
            }
        });
        view.setDownloadListener((url, agent, disposition, mime, length) ->
                dispose(StopReason.DOWNLOAD_FORBIDDEN));
        view.addJavascriptInterface(new NativeBridge(), BRIDGE);
        // One invisible, noninteractive pixel outside the native editor bounds.
        // The original NativeActivity surface and Slint input are not replaced.
        view.setAlpha(0f);
        view.setFocusable(false);
        view.setFocusableInTouchMode(false);
        view.setClickable(false);
        view.setSaveEnabled(false);
        view.setSaveFromParentEnabled(false);
        view.setImportantForAccessibility(View.IMPORTANT_FOR_ACCESSIBILITY_NO_HIDE_DESCENDANTS);
        FrameLayout.LayoutParams bounds = new FrameLayout.LayoutParams(1, 1);
        bounds.leftMargin = -2;
        bounds.topMargin = -2;
        activity.addContentView(view, bounds);
        view.loadUrl(BKCatRuntimePolicy.DOCUMENT);
    }

    /** Only this method is visible to the controlled document. */
    public final class NativeBridge {
        @JavascriptInterface
        public void postMessage(final String message) {
            if (!policy.active()) return;
            if (message == null || message.length() > BKCatRuntimePolicy.MAX_MESSAGE_CHARS
                    || message.getBytes(StandardCharsets.UTF_8).length
                            > BKCatRuntimePolicy.MAX_MESSAGE_CHARS) {
                if (!UI.post(() -> dispose(StopReason.MESSAGE_TOO_LARGE))) {
                    queueUnavailable(StopReason.UI_MESSAGE_REJECTED);
                }
                return;
            }
            // JavaScript-interface calls arrive on WebView's background thread.
            // A pause/navigation posted ahead of this callback wins on the UI
            // thread; no stale callback is forwarded to the native dispatcher.
            if (!UI.post(() -> {
                if (webView != null && policy.canMessage(webView.getUrl(), message)) {
                    runtimeMessage(instance, message);
                }
            })) queueUnavailable(StopReason.UI_MESSAGE_REJECTED);
        }
    }

    // Called from a bridge thread only when the UI queue cannot accept work.
    // Revoke synchronously; no off-thread WebView access or fake destruction.
    private void queueUnavailable(StopReason reason) {
        policy.retire();
        reportStopping(instance, reason);
    }

    private final class ControlledClient extends WebViewClient {
        @Override public boolean shouldOverrideUrlLoading(
                WebView view, WebResourceRequest request) {
            dispose(StopReason.NAVIGATION_FORBIDDEN);
            return true;
        }
        @Override public boolean shouldOverrideUrlLoading(WebView view, String url) {
            dispose(StopReason.NAVIGATION_FORBIDDEN);
            return true;
        }
        @Override public void onPageStarted(WebView view, String url, Bitmap favicon) {
            if (!policy.beginDocument(url)) {
                dispose(StopReason.DOCUMENT_CHANGED);
            }
        }
        @Override public void doUpdateVisitedHistory(WebView view, String url, boolean reload) {
            if (reload || !BKCatRuntimePolicy.DOCUMENT.equals(url)) {
                dispose(StopReason.HISTORY_CHANGED);
            }
        }
        @Override public WebResourceResponse shouldInterceptRequest(
                WebView view, WebResourceRequest request) {
            return resource(request.getUrl().toString(), request.getMethod());
        }
        @Override public WebResourceResponse shouldInterceptRequest(WebView view, String url) {
            return resource(url, "GET");
        }
        @Override public void onReceivedSslError(
                WebView view, SslErrorHandler handler, SslError error) {
            handler.cancel();
            dispose(StopReason.TLS_FORBIDDEN);
        }
        @Override public void onReceivedError(WebView view,
                WebResourceRequest request, WebResourceError error) {
            if (request.isForMainFrame()) dispose(StopReason.DOCUMENT_LOAD_FAILED);
        }
        @Override public void onReceivedHttpError(WebView view,
                WebResourceRequest request, WebResourceResponse error) {
            if (request.isForMainFrame()) dispose(StopReason.DOCUMENT_LOAD_FAILED);
        }
        @Override public boolean onRenderProcessGone(
                WebView view, RenderProcessGoneDetail detail) {
            dispose(StopReason.RENDERER_EXITED);
            return true;
        }
    }

    /** Always returns a response: null would let WebView use the network. */
    private WebResourceResponse resource(String url, String method) {
        String path = BKCatRuntimePolicy.resourcePath(url, method);
        byte[] data = policy.active() && path != null && resources != null
                ? resources.get(path) : null;
        if (data == null) {
            return new WebResourceResponse("text/plain", "UTF-8", 403, "Forbidden",
                    Collections.singletonMap("Cache-Control", "no-store"),
                    new ByteArrayInputStream(new byte[0]));
        }
        Map<String, String> headers = new HashMap<>();
        headers.put("Cache-Control", "no-store");
        headers.put("X-Content-Type-Options", "nosniff");
        headers.put("Content-Security-Policy",
                "default-src 'none'; script-src 'self'; connect-src 'none'; "
                + "frame-src 'none'; worker-src 'none'; object-src 'none'; "
                + "base-uri 'none'; form-action 'none'; frame-ancestors 'none'");
        return new WebResourceResponse("/index.html".equals(path)
                ? "text/html" : "text/javascript", "UTF-8", 200, "OK", headers,
                new ByteArrayInputStream(data));
    }

    /**
     * A plain observer is never serialized into Activity/Fragment saved state.
     * The dynamically loaded runtime class must never enter FragmentManager:
     * Android restores fragments through the Activity's different class loader.
     */
    private static final class Lifecycle implements Application.ActivityLifecycleCallbacks {
        final Activity activity;
        boolean paused;

        Lifecycle(Activity activity) { this.activity = activity; }

        void retire(StopReason reason) {
            paused = true;
            for (BKCatRuntime host : INSTANCES.values().toArray(new BKCatRuntime[0])) {
                if (host.activity == activity) host.dispose(reason);
            }
        }

        @Override public void onActivityCreated(Activity value, Bundle savedState) {}
        @Override public void onActivityStarted(Activity value) {}
        @Override public void onActivityResumed(Activity value) {
            // Only explicit native create() can create another component.
            if (value == activity) paused = false;
        }
        @Override public void onActivityPaused(Activity value) {
            if (value == activity) retire(StopReason.ACTIVITY_PAUSED);
        }
        @Override public void onActivityStopped(Activity value) {
            if (value == activity) retire(StopReason.ACTIVITY_STOPPED);
        }
        @Override public void onActivitySaveInstanceState(Activity value, Bundle state) {
            // This can precede onPause on supported older Android versions.
            // Nothing about the runtime, its capability or queue is serialized.
            if (value == activity) retire(StopReason.ACTIVITY_SAVED);
        }
        @Override public void onActivityDestroyed(Activity value) {
            if (value != activity) return;
            retire(StopReason.ACTIVITY_DESTROYED);
            activity.getApplication().unregisterActivityLifecycleCallbacks(this);
            if (lifecycle == this) lifecycle = null;
        }
    }

    private void dispose(StopReason reason) {
        policy.retire();
        initialCreation.retire();
        if (!disposed.compareAndSet(false, true)) return;
        // First revoke native execution, but retain its storage ownership until
        // destroy() actually returns. JS unload handlers are not a handoff gate.
        if (instance != null) reportStopping(instance, reason);
        clearInitialWait();
        WebView old = webView;
        webView = null;
        if (old != null) {
            try {
                old.removeJavascriptInterface(BRIDGE);
                old.stopLoading();
                ViewGroup parent = (ViewGroup) old.getParent();
                if (parent != null) parent.removeView(old);
                old.destroy();
            } catch (RuntimeException error) {
                // Fail closed: no "closed" acknowledgement, and no replacement
                // component. Native keeps the revoked owner's storage locked.
                Log.e("concat", "bk-cat-android-host: state=cleanup reason=webview-destruction-failed");
                return;
            }
        }
        if (instance != null) {
            INSTANCES.remove(instance);
            reportClosed(instance, reason);
        }
    }
}
