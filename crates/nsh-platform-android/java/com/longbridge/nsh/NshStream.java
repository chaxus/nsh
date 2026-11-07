package com.longbridge.nsh;

import android.view.Surface;

/**
 * A live stream the app registered ({@link NshView#registerStream}) for
 * {@code <Video srcObject={MediaStream.fromHost(name)}>} in JS: the video
 * naming it hands its {@link Surface} to the stream's {@link Producer}, which
 * draws the frames into it from any thread (a {@code Canvas}, a
 * {@code MediaCodec}, a camera, a call's SDK). The video's {@code objectFit}
 * fits the frames; its first frame is {@code onLoad}. The stream's own
 * methods are called on the UI thread.
 *
 * A surface belongs to one {@code TextureView}, so a stream shows in one
 * video at a time: a second video naming it shows nothing (logged once).
 */
public final class NshStream {
    /** Draws the stream's frames into the surface a video hands it. */
    public interface Producer {
        /**
         * The surface of the video showing the stream: draw into it from
         * now on. UI thread; draw from any thread.
         *
         * @param surface the surface, NSH's until {@link #onSurfaceGone}
         */
        void onSurface(Surface surface);

        /**
         * The surface is gone (the video was removed, named another stream,
         * lost its texture, or the stream ended): stop drawing into it
         * before returning, since it is released next. UI thread.
         */
        void onSurfaceGone();
    }

    private final Mounter mounter;
    final Mounter.Stream stream;
    final Producer producer;

    NshStream(Mounter mounter, Mounter.Stream stream, Producer producer) {
        this.mounter = mounter;
        this.stream = stream;
        this.producer = producer;
    }

    /**
     * The name JS uses.
     *
     * @return the name
     */
    public String name() {
        return stream.name;
    }

    /**
     * The frames' size, in pixels: the surface's buffers take it (a
     * {@code Canvas} locked on it is this size), the video fits them by its
     * {@code objectFit} and reports it in {@code onLoad}. Until set, the
     * frames fill the video and {@code onLoad} reports the video's size:
     * set it right after registering, before the first frame is drawn (a
     * video already naming the stream has its surface by then).
     *
     * @param width the frames' width
     * @param height the frames' height
     */
    public void setSize(int width, int height) {
        mounter.streamSize(this, width, height);
    }

    /**
     * Ends the stream: the producer hears {@link Producer#onSurfaceGone} if
     * it holds a surface, and the videos naming the stream show nothing
     * until the name is registered again. Nothing after a later
     * registration of the name.
     */
    public void end() {
        mounter.endStream(this);
    }
}
