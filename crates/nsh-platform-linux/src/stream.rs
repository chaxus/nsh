//! Live streams the embedding app registers by name, which `<Video
//! srcObject>` plays (`MediaStream.fromHost(name)`, `VideoProps::stream`):
//! the host pushes frames (BGRA bytes) from any thread, and each video
//! showing the stream draws them as a `gdk::MemoryTexture` in its
//! `gtk::Picture`, on the main thread. The frames never pass through JS.
//!
//! The rules every backend shares (one entry per name whether the host or
//! a video came first, the newest frame wins, paused sinks keep their last
//! frame, a name nothing registered is logged once, a frame that does not
//! fit its size shows nothing) are [`nsh_core::stream`]'s; this module
//! gives it the frame GTK draws, a [`Frame`] whose bytes are a
//! `glib::Bytes` the videos' textures share instead of copying again, and
//! the hop to GLib's default main context, where the sinks (a video's
//! picture and emitter) are told.

use std::sync::Arc;

use gtk4::{self as gtk, glib};

/// A frame: `width` × `height` pixels of BGRA (8 bits each, opaque or
/// premultiplied), `stride` bytes per row.
pub(crate) struct Frame {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride: usize,
    /// `stride * height` bytes; a `glib::Bytes` so the videos' textures
    /// share them instead of copying them again.
    pub(crate) bytes: glib::Bytes,
}

impl Frame {
    /// A frame of `bytes`, copied: the core's `make` ([`StreamHandle::push_bgra`]),
    /// called once the size is checked (`stride * height` bytes are there).
    fn make(width: usize, height: usize, stride: usize, bytes: &[u8]) -> Option<Self> {
        Some(Frame {
            width,
            height,
            stride,
            bytes: glib::Bytes::from(&bytes[..stride * height]),
        })
    }

    /// The frame as a texture (main thread).
    pub(crate) fn texture(&self) -> gtk::gdk::MemoryTexture {
        gtk::gdk::MemoryTexture::new(
            self.width as i32,
            self.height as i32,
            gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &self.bytes,
            self.stride,
        )
    }
}

/// What a stream tells the videos showing it.
pub(crate) type StreamEvent<'a> = nsh_core::stream::StreamEvent<'a, Frame>;

/// A stream the embedding app registered ([`Streams::register`]): where it
/// pushes the frames. Any thread. Dropping it ends the stream.
pub struct StreamHandle(nsh_core::stream::StreamHandle<Frame>);

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl StreamHandle {
    /// The name it was registered as.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// Shows a frame in the videos playing the stream: `width` × `height`
    /// pixels of BGRA (8 bits each, opaque or premultiplied), `stride`
    /// bytes per row (at least `width * 4`). Copies the bytes. A frame that
    /// comes before the previous one was drawn replaces it. A frame of a
    /// size no frame can have (zero, a stride shorter than its rows, fewer
    /// bytes than its rows) shows nothing, logged once per stream.
    pub fn push_bgra(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) {
        self.0.push_bgra(width, height, stride, bytes, Frame::make);
    }

    /// Ends the stream: the videos playing it show nothing from now on,
    /// until the name is registered again.
    pub fn end(&self) {
        self.0.end();
    }
}

/// A video's attachment to a stream, by name ([`Streams::attach`]);
/// dropping it detaches the video's sink. Main thread.
pub(crate) struct Attached(nsh_core::stream::Attached<Frame>);

impl Attached {
    /// The stream's name.
    pub(crate) fn name(&self) -> &Arc<str> {
        self.0.name()
    }

    /// Stops or resumes telling the sink the frames (the last one stays; a
    /// sink with none yet still gets the first).
    pub(crate) fn set_paused(&self, paused: bool) {
        self.0.set_paused(paused);
    }
}

/// The streams of one surface, by name (shared with its videos, so a
/// registration applies to videos showing the name from then on, and to
/// those already showing it). Main thread.
#[derive(Clone)]
pub(crate) struct Streams(nsh_core::stream::Streams<Frame>);

impl Default for Streams {
    fn default() -> Self {
        Streams(nsh_core::stream::Streams::new(
            |job: Box<dyn FnOnce() + Send>| {
                glib::MainContext::default().invoke(job);
                true
            },
        ))
    }
}

impl std::fmt::Debug for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl Streams {
    /// Registers stream `name` (replacing an earlier registration): the
    /// handle takes its frames. The videos whose `srcObject` names it,
    /// now or later, play it.
    pub(crate) fn register(&self, name: &str) -> StreamHandle {
        StreamHandle(self.0.register(name))
    }

    /// Attaches `sink`, a video's, to stream `name`: it hears the frames
    /// and the end. A name nothing registered is logged once; the sink
    /// hears frames once something is.
    pub(crate) fn attach(&self, name: &str, sink: Box<dyn Fn(StreamEvent<'_>)>) -> Attached {
        Attached(self.0.attach(name, sink))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_keeps_its_size_and_rows_of_bytes() {
        // Three rows of a 12-byte stride, with 8 bytes of slack behind.
        let bytes = vec![7u8; 12 * 3 + 8];
        let frame = Frame::make(2, 3, 12, &bytes).unwrap();
        assert_eq!((frame.width, frame.height, frame.stride), (2, 3, 12));
        assert_eq!(frame.bytes.len(), 36);
        assert!(frame.bytes.iter().all(|b| *b == 7));
    }

    // Registering and attaching need no main loop: a push would hop to
    // GLib's default main context, so none is made here.
    #[test]
    fn registers_and_attaches_by_name() {
        let streams = Streams::default();
        let attached = streams.attach("cam", Box::new(|_: StreamEvent<'_>| {}));
        let handle = streams.register("cam");
        assert_eq!(handle.name(), "cam");
        assert_eq!(&**attached.name(), "cam");
        assert_eq!(format!("{streams:?}"), r#"{"cam"}"#);
        assert_eq!(format!("{handle:?}"), r#"StreamHandle { name: "cam" }"#);
    }
}
