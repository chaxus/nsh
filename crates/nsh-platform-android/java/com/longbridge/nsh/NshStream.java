package com.longbridge.nsh;

import android.view.Surface;
import java.nio.ByteBuffer;

/**
 * A live stream the app registered ({@link NshView#registerStream}) for
 * {@code <Video srcObject={MediaStream.fromHost(name)}>} in JS, in one of
 * two forms:
 *
 * <ul>
 * <li><b>Frames</b> ({@link NshView#registerStream(String)}): the app pushes
 * BGRA frames from any thread ({@link #pushBgra}), as the iOS, macOS,
 * Windows, Linux and GPUI SDKs take them; every video naming the stream
 * draws them, fit by its {@code objectFit}, {@code paused} keeps the last
 * one, the first is {@code onLoad}.</li>
 * <li><b>A surface</b> ({@link NshView#registerStream(String, Producer)}):
 * the video naming the stream hands its {@link Surface} to the stream's
 * {@link Producer}, which draws into it directly (a {@code MediaCodec}, a
 * camera, a call's SDK), with no copy. A surface belongs to one
 * {@code TextureView}, so a producer's stream shows in one video at a
 * time: a second video naming it waits (logged once) and takes over when
 * the first stops showing it; and the producer draws on its own clock,
 * so {@code paused} leaves the stream playing.</li>
 * </ul>
 *
 * The stream's own methods but {@link #pushBgra} are called on the UI thread.
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
    /** The producer, or null for a stream that takes frames. */
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
     * Shows a frame in the videos naming the stream: {@code width} ×
     * {@code height} pixels of BGRA (8 bits each, opaque or premultiplied),
     * {@code stride} bytes per row (at least {@code width * 4}), read from
     * {@code bytes}'s position on, and copied. Any thread. A frame that
     * comes before the previous one was drawn replaces it (a live stream
     * shows the newest). A frame that does not fit its size (or has none)
     * shows nothing, logged once per stream; so does a frame pushed to a
     * stream registered with a {@link Producer}. While no video shows the
     * stream the bytes are not copied.
     *
     * @param bytes the pixels, {@code stride * height} bytes from its position
     * @param width the frame's width in pixels
     * @param height the frame's height in pixels
     * @param stride the bytes per row
     */
    public void pushBgra(ByteBuffer bytes, int width, int height, int stride) {
        stream.pushBgra(this, bytes, width, height, stride);
    }

    /**
     * The frames' size, in pixels, for a {@link Producer}'s stream: the
     * surface's buffers take it (a {@code Canvas} locked on it is this
     * size), the video fits them by its {@code objectFit} and reports it in
     * {@code onLoad}. Until set, the frames fill the video and
     * {@code onLoad} reports the video's size: set it right after
     * registering, before the first frame is drawn (a video already naming
     * the stream has its surface by then). A stream that takes frames has
     * their size already: this does nothing for it.
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
