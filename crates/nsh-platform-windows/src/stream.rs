//! Live streams the embedding app registers by name, which `<Video
//! srcObject>` plays (`MediaStream.fromHost(name)`, `VideoProps::stream`):
//! the host pushes frames (BGRA bytes) from any thread, and each video
//! showing the stream copies them into a Direct2D bitmap of its own on the
//! UI thread ([`crate::native`]'s `video`). The frames never pass through
//! JS.
//!
//! The rules every backend shares (one entry per name whichever side comes
//! first, the newest frame wins, paused sinks, the end of a registration,
//! a frame that does not fit its size, a name nothing registered) live in
//! [`nsh_core::stream`]; this module is the Windows side of it: a frame is
//! its BGRA bytes copied into a `Vec` ([`Frame`]), and the sinks hear the
//! frames on the UI thread, reached with [`UiThread::invoke`]. The UI thread
//! is captured when the first video attaches: a registry may be made before
//! its window's thread runs its message loop.

use std::sync::{Arc, Mutex};

use crate::window::UiThread;

/// One frame of a stream: `height` rows of `stride` bytes, each `width`
/// BGRA pixels (8 bits each, premultiplied or opaque).
pub(crate) struct Frame {
    /// Pixels.
    pub(crate) width: usize,
    /// Pixels.
    pub(crate) height: usize,
    /// Bytes per row (at least `width * 4`).
    pub(crate) stride: usize,
    /// At least `stride * height` bytes.
    pub(crate) bytes: Vec<u8>,
}

/// What a video attached to a stream hears, on the UI thread.
pub(crate) type StreamEvent<'a> = nsh_core::stream::StreamEvent<'a, Frame>;

/// The hop to the UI thread the sinks live on: the thread that attached
/// the first video (`None` until one does; the registry is one thread's).
#[derive(Clone, Default)]
struct UiHop {
    ui: Arc<Mutex<Option<UiThread>>>,
}

impl UiHop {
    /// Makes the calling thread the sinks' thread, if none is yet.
    fn capture(&self) {
        self.ui
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(UiThread::current);
    }

    /// The registry's hop ([`nsh_core::stream::Hop`]): runs `job` on the
    /// UI thread; `false` while no video attached yet, or when the thread
    /// cannot take it (the registry tries again with the next push).
    fn post(&self, job: Box<dyn FnOnce() + Send>) -> bool {
        let ui = *self.ui.lock().unwrap_or_else(|e| e.into_inner());
        ui.is_some_and(|ui| ui.invoke(job))
    }
}

/// A stream the embedding app registered ([`crate::AppView::register_stream`]):
/// where it pushes the frames. Any thread. Dropping it ends the stream.
pub struct StreamHandle(nsh_core::stream::StreamHandle<Frame>);

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle")
            .field("name", &self.0.name())
            .finish()
    }
}

impl StreamHandle {
    /// The name it was registered as.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// Shows a frame in the videos playing the stream: `width` × `height`
    /// pixels of BGRA (8 bits each, opaque or premultiplied), `stride`
    /// bytes per row (at least `width * 4`). Copies the bytes. A frame
    /// that comes before the previous one was drawn replaces it. A frame
    /// that does not add up (`bytes` shorter than `stride * height`, a
    /// stride under `width * 4`, an empty size) is logged once and shows
    /// nothing.
    pub fn push_bgra(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) {
        self.0.push_bgra(
            width,
            height,
            stride,
            bytes,
            |width, height, stride, bytes| {
                Some(Frame {
                    width,
                    height,
                    stride,
                    bytes: bytes[..stride * height].to_vec(),
                })
            },
        );
    }

    /// Ends the stream: the videos playing it show nothing from now on,
    /// until the name is registered again.
    pub fn end(&self) {
        self.0.end();
    }
}

/// A video's attachment to a stream, by name ([`Streams::attach`]);
/// dropping it detaches the video's sink.
pub(crate) struct Attached(nsh_core::stream::Attached<Frame>);

impl Attached {
    /// The stream's name.
    pub(crate) fn name(&self) -> &str {
        self.0.name()
    }

    /// Stops or resumes delivering frames to the video's sink (the last
    /// one it drew stays; with none drawn yet, the first to come is).
    pub(crate) fn set_paused(&self, paused: bool) {
        self.0.set_paused(paused);
    }
}

/// The streams of one registry ([`crate::Components`]), by name (shared
/// with its videos, so a registration applies to videos showing the name
/// from then on, and to those already showing it). One thread's: the UI
/// thread of the surfaces made with the registry.
#[derive(Clone)]
pub(crate) struct Streams {
    inner: nsh_core::stream::Streams<Frame>,
    ui: UiHop,
}

impl Default for Streams {
    fn default() -> Self {
        let ui = UiHop::default();
        let hop = ui.clone();
        Streams {
            inner: nsh_core::stream::Streams::new(move |job: Box<dyn FnOnce() + Send>| {
                hop.post(job)
            }),
            ui,
        }
    }
}

impl std::fmt::Debug for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl Streams {
    /// Registers stream `name` (replacing an earlier registration): the
    /// handle takes its frames. The videos whose `srcObject` names it, now
    /// or later, play it.
    pub(crate) fn register(&self, name: &str) -> StreamHandle {
        StreamHandle(self.inner.register(name))
    }

    /// Attaches `sink`, a video's, to stream `name`: it hears the frames
    /// and the end on this thread, the UI thread of the video's surface. A
    /// name nothing registered is logged once; the sink hears once
    /// something is.
    pub(crate) fn attach(&self, name: &str, sink: Box<dyn Fn(StreamEvent<'_>)>) -> Attached {
        self.ui.capture();
        Attached(self.inner.attach(name, sink))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    #[test]
    fn a_frame_is_the_bytes_of_stride_times_height() {
        let frames = Rc::new(RefCell::new(Vec::new()));
        let seen = frames.clone();
        let streams = nsh_core::stream::Streams::<Frame>::new(|job: Box<dyn FnOnce() + Send>| {
            job();
            true
        });
        let _attached = streams.attach(
            "cam",
            Box::new(move |event: StreamEvent<'_>| {
                if let StreamEvent::Frame(f) = event {
                    seen.borrow_mut()
                        .push((f.width, f.height, f.stride, f.bytes.clone()));
                }
            }),
        );
        let handle = StreamHandle(streams.register("cam"));
        // A padded stride, and more bytes than the frame: exactly stride ×
        // height of them are copied.
        let mut bytes = vec![0; 40];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        handle.push_bgra(2, 2, 12, &bytes);
        let frames = frames.borrow();
        assert_eq!(frames.len(), 1);
        let (width, height, stride, copied) = &frames[0];
        assert_eq!((*width, *height, *stride), (2, 2, 12));
        assert_eq!(copied.len(), 24);
        assert_eq!(&copied[..], &bytes[..24]);
    }

    #[test]
    fn a_registry_registers_and_attaches_without_a_message_loop() {
        let streams = Streams::default();
        let attached = streams.attach("cam", Box::new(|_| {}));
        assert_eq!(attached.name(), "cam");
        let handle = streams.register("cam");
        assert_eq!(handle.name(), "cam");
        assert_eq!(format!("{streams:?}"), r#"{"cam"}"#);
        assert_eq!(format!("{handle:?}"), r#"StreamHandle { name: "cam" }"#);
        // Delivered through the UI thread's hop, which needs a message
        // loop: here it only must not panic.
        handle.push_bgra(1, 1, 4, &[1, 2, 3, 255]);
        attached.set_paused(true);
        handle.end();
    }
}
