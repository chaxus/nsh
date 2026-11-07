package com.longbridge.nsh;

import android.app.Activity;
import android.os.Build;
import android.os.Bundle;

/**
 * The development host: runs the app served by `nsh dev` (`nsh run --target
 * android` passes the server URL as the `url` extra). With a `package` extra
 * (a `.nshpkg` path on the device) and `root` (a root public key, base64;
 * comma-separated for several) it verifies and runs that signed package as an
 * embedding app would; its trust state lives in the cache directory.
 */
public final class DevActivity extends Activity {
    private NshView view;
    private NshTrust trust;

    /** Created by the system from the manifest. */
    public DevActivity() {}

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        edgeToEdge(this);
        boolean trace = getIntent().getBooleanExtra("trace", false);
        // A test session's app (`nsh test --target android`) keeps its data
        // in a directory of its own, empty when it starts.
        String data = getIntent().getStringExtra("dataDir");
        if (data != null) {
            java.io.File dir = new java.io.File(new java.io.File(getCacheDir(), "nsh-test"), data);
            deleteTree(dir);
            // Its caches too (downloaded images): the app starts with none.
            view = new NshView(this, trace, dir, new java.io.File(dir, "cache"));
        } else {
            view = new NshView(this, trace);
        }
        // An app on its own, not a module embedded in another app: no limits.
        view.useStandaloneLimits();
        setContentView(view);
        // A live stream the tests play (`<Video srcObject={MediaStream.fromHost("nsh/test-pattern")}>`).
        view.registerStream("nsh/test-pattern", new TestPattern()).setSize(TestPattern.WIDTH, TestPattern.HEIGHT);
        view.handleNotificationIntent(getIntent());
        // A recreated activity's intent was handed in already.
        if (state == null) view.handleUrlSchemeIntent(getIntent());
        String pkg = getIntent().getStringExtra("package");
        if (pkg != null) {
            trust = new NshTrust(new java.io.File(getCacheDir(), "nsh-trust"));
            String roots = getIntent().getStringExtra("root");
            if (roots != null) {
                for (String root : roots.split(",")) trust.addRoot(root);
            }
            try {
                view.runPackage(trust.openPackage(new java.io.File(pkg)));
            } catch (NshPackage.RefusedException e) {
                view.reportError(e.getMessage());
            }
            return;
        }
        String url = getIntent().getStringExtra("url");
        view.connectDevServer(url != null ? url : "ws://127.0.0.1:8790/ws");
    }

    /**
     * The stream `nsh/test-pattern`: a hue sweeping through the colors behind
     * a white bar moving right, drawn with a `Canvas` on a thread of its own
     * at about 30 frames per second. The pattern is {@link #WIDTH} by
     * {@link #HEIGHT} and scales to the canvas it gets.
     */
    static final class TestPattern implements NshStream.Producer {
        static final int WIDTH = 320, HEIGHT = 180;
        private static final int BAR = 16, STEP = 4, FRAME_MS = 33;
        private final android.graphics.Paint bar = new android.graphics.Paint();
        private android.os.HandlerThread thread;
        private android.os.Handler handler;
        private volatile android.view.Surface surface;
        private int tick;

        TestPattern() {
            bar.setColor(android.graphics.Color.WHITE);
        }

        @Override
        public void onSurface(android.view.Surface surface) {
            this.surface = surface;
            thread = new android.os.HandlerThread("nsh-test-pattern");
            thread.start();
            handler = new android.os.Handler(thread.getLooper());
            handler.post(this::frame);
        }

        @Override
        public void onSurfaceGone() {
            // Not joined: `lockCanvas` blocks while the view is not drawn
            // (hidden, detached), and the UI thread must not wait on it. A
            // frame in progress ends on the released surface, which
            // `frame` tolerates.
            surface = null;
            thread.quit();
            thread = null;
            handler = null;
        }

        private void frame() {
            android.view.Surface s = surface;
            android.os.Handler h = handler;
            if (s == null || h == null) return;
            try {
                android.graphics.Canvas c = s.lockCanvas(null);
                float sx = c.getWidth() / (float) WIDTH, sy = c.getHeight() / (float) HEIGHT;
                c.drawColor(android.graphics.Color.HSVToColor(new float[] {tick % 360, 1, 1}));
                float x = (tick * STEP % WIDTH) * sx;
                c.drawRect(x, 0, x + BAR * sx, HEIGHT * sy, bar);
                s.unlockCanvasAndPost(c);
            } catch (IllegalArgumentException | android.view.Surface.OutOfResourcesException e) {
                // The surface went (or has no buffer) between the check and
                // the lock: the next frame finds `surface` cleared.
            }
            tick++;
            h.postDelayed(this::frame, FRAME_MS);
        }
    }

    private static void deleteTree(java.io.File file) {
        java.io.File[] children = file.listFiles();
        if (children != null) for (java.io.File c : children) deleteTree(c);
        file.delete();
    }

    /** A notification pressed, or a URL opened, while this activity is in its task. */
    @Override
    protected void onNewIntent(android.content.Intent intent) {
        super.onNewIntent(intent);
        if (view != null) {
            view.handleNotificationIntent(intent);
            view.handleUrlSchemeIntent(intent);
        }
    }

    /** Android 12 and older: the app gets the back button first. */
    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        if (view == null || !view.handleBack()) super.onBackPressed();
    }

    @Override
    protected void onDestroy() {
        view.destroy();
        if (trust != null) trust.close();
        super.onDestroy();
    }

    @Override
    public void onConfigurationChanged(android.content.res.Configuration config) {
        super.onConfigurationChanged(config);
        systemBars(this);
    }

    /** Draw behind the system bars: the app gets them as `safeArea()`. */
    @SuppressWarnings("deprecation")
    static void edgeToEdge(Activity activity) {
        if (Build.VERSION.SDK_INT >= 30) {
            activity.getWindow().setDecorFitsSystemWindows(false);
        } else {
            activity.getWindow().getDecorView().setSystemUiVisibility(
                    android.view.View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | android.view.View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | android.view.View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
        }
        activity.getWindow().setStatusBarColor(0);
        activity.getWindow().setNavigationBarColor(0);
        systemBars(activity);
    }

    /** Dark system bar icons in light mode, light ones in dark mode. */
    @SuppressWarnings("deprecation")
    static void systemBars(Activity activity) {
        int night = activity.getResources().getConfiguration().uiMode
                & android.content.res.Configuration.UI_MODE_NIGHT_MASK;
        boolean dark = night == android.content.res.Configuration.UI_MODE_NIGHT_YES;
        // Creates the decor view, which owns the insets controller.
        activity.getWindow().getDecorView();
        if (Build.VERSION.SDK_INT >= 30) {
            android.view.WindowInsetsController c = activity.getWindow().getInsetsController();
            if (c == null) return;
            int light = android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                    | android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS;
            c.setSystemBarsAppearance(dark ? 0 : light, light);
        } else {
            android.view.View decor = activity.getWindow().getDecorView();
            int flags = decor.getSystemUiVisibility();
            int light = android.view.View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR;
            decor.setSystemUiVisibility(dark ? flags & ~light : flags | light);
        }
    }
}
