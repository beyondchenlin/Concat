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
import android.view.View;
import android.view.ViewGroup;
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
                lifecycle.retire("Android activity replaced");
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
            if (activity.isFinishing() || activity.isDestroyed() || !activity.hasWindowFocus()
                    || lifecycle == null || lifecycle.activity != activity || lifecycle.paused) {
                runtimeClosed(instance, "android activity is not foreground-ready");
                return;
            }
            if (!INSTANCES.isEmpty()) {
                runtimeClosed(instance, "the preceding Android runtime has not closed");
                return;
            }
            Map<String, byte[]> assets = new HashMap<>();
            assets.put("/index.html", document.getBytes(StandardCharsets.UTF_8));
            assets.put("/runtime.js", runtime.getBytes(StandardCharsets.UTF_8));
            assets.put("/bootstrap.js", bootstrap.getBytes(StandardCharsets.UTF_8));
            assets.put("/transport.js", transport.getBytes(StandardCharsets.UTF_8));
            BKCatRuntime host = new BKCatRuntime(activity, instance, Collections.unmodifiableMap(assets));
            INSTANCES.put(instance, host);
            try {
                host.openView();
            } catch (RuntimeException error) {
                host.dispose("Android WebView creation failed");
            }
        })) runtimeClosed(instance, "Android UI rejected runtime creation");
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
                fragment.dispose("Android runtime rejected native delivery");
            }
        })) throw new IllegalStateException("Android UI rejected native delivery");
    }

    public static void close(final String instance) {
        if (!UI.post(() -> {
            BKCatRuntime fragment = INSTANCES.get(instance);
            if (fragment != null) fragment.dispose("native host closed");
        })) runtimeStopping(instance, "Android UI could not acknowledge destruction");
    }

    @SuppressLint("SetJavaScriptEnabled")
    private void openView() {
        if (!policy.active()) {
            dispose("Android runtime has no active activity");
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
                dispose("a runtime document attempted a download"));
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
                if (!UI.post(() -> dispose("Android runtime exceeded its message bound"))) {
                    queueUnavailable("Android UI refused an oversized runtime message");
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
            })) queueUnavailable("Android UI refused a runtime message");
        }
    }

    // Called from a bridge thread only when the UI queue cannot accept work.
    // Revoke synchronously; no off-thread WebView access or fake destruction.
    private void queueUnavailable(String reason) {
        policy.retire();
        runtimeStopping(instance, reason);
    }

    private final class ControlledClient extends WebViewClient {
        @Override public boolean shouldOverrideUrlLoading(
                WebView view, WebResourceRequest request) {
            dispose("runtime navigation is forbidden");
            return true;
        }
        @Override public boolean shouldOverrideUrlLoading(WebView view, String url) {
            dispose("runtime navigation is forbidden");
            return true;
        }
        @Override public void onPageStarted(WebView view, String url, Bitmap favicon) {
            if (!policy.beginDocument(url)) {
                dispose("runtime document identity changed");
            }
        }
        @Override public void doUpdateVisitedHistory(WebView view, String url, boolean reload) {
            if (reload || !BKCatRuntimePolicy.DOCUMENT.equals(url)) {
                dispose("runtime document history changed");
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
            dispose("runtime TLS request is forbidden");
        }
        @Override public void onReceivedError(WebView view,
                WebResourceRequest request, WebResourceError error) {
            if (request.isForMainFrame()) dispose("runtime document failed to load");
        }
        @Override public void onReceivedHttpError(WebView view,
                WebResourceRequest request, WebResourceResponse error) {
            if (request.isForMainFrame()) dispose("runtime document failed to load");
        }
        @Override public boolean onRenderProcessGone(
                WebView view, RenderProcessGoneDetail detail) {
            dispose("Android runtime renderer exited; native recovery is required");
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

        void retire(String reason) {
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
            if (value == activity) retire("Android activity paused; explicit recovery is required");
        }
        @Override public void onActivityStopped(Activity value) {
            if (value == activity) retire("Android activity stopped");
        }
        @Override public void onActivitySaveInstanceState(Activity value, Bundle state) {
            // This can precede onPause on supported older Android versions.
            // Nothing about the runtime, its capability or queue is serialized.
            if (value == activity) retire("Android activity state saved; explicit recovery is required");
        }
        @Override public void onActivityDestroyed(Activity value) {
            if (value != activity) return;
            retire("Android activity destroyed");
            activity.getApplication().unregisterActivityLifecycleCallbacks(this);
            if (lifecycle == this) lifecycle = null;
        }
    }

    private void dispose(String reason) {
        policy.retire();
        if (!disposed.compareAndSet(false, true)) return;
        // First revoke native execution, but retain its storage ownership until
        // destroy() actually returns. JS unload handlers are not a handoff gate.
        if (instance != null) runtimeStopping(instance, reason);
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
                return;
            }
        }
        if (instance != null) {
            INSTANCES.remove(instance);
            runtimeClosed(instance, reason);
        }
    }
}
