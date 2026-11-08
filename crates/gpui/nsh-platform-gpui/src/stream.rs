//! Live streams the embedding app registers by name, which `<Video
//! srcObject>` plays (`MediaStream.fromHost(name)`, `VideoProps::stream`):
//! the host pushes BGRA frames from any thread, and each video showing the
//! stream paints them as its picture, on the main thread. The frames never
//! pass through JS.
//!
//! The registry is the core's ([`nsh_core::stream`]: one entry per name
//! whether the host or a video came first, the newest frame wins, paused
//! sinks keep their last frame, a wrong push is logged once, a push nobody
//! shows costs nothing). This module gives it GPUI's frame: a frame
//! becomes a GPUI image ([`RenderImage`]) on the pushing thread, once,
//! however many videos show it; the videos hear it through the sinks they
//! attached ([`Streams::attach`]), on the main thread, one hop
//! ([`crate::rt::invoke`]) per batch of frames. A frame pushed before the
//! app runs (a host registering at start) waits for the next push rather
//! than wedging the stream: the hop reports that it queued nothing and the
//! core schedules again with the next push.

use std::sync::Arc;

use gpui::RenderImage;

/// A frame of a stream: its size (pixels) and the GPUI image of it, BGRA.
pub(crate) struct Frame {
    /// Width in pixels.
    pub(crate) width: u32,
    /// Height in pixels.
    pub(crate) height: u32,
    /// The picture, shared by every video painting it.
    pub(crate) image: Arc<RenderImage>,
}

/// What a stream tells the videos showing it.
pub(crate) type StreamEvent<'a> = nsh_core::stream::StreamEvent<'a, Frame>;

/// A stream the embedding app registered ([`Streams::register`],
/// [`crate::AppView::register_stream`]): where it pushes the frames. Any
/// thread. Dropping it ends the stream.
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
    /// that does not fit its size (or has none) is a wrong use: it shows
    /// nothing, logged once per stream.
    pub fn push_bgra(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) {
        self.0
            .push_bgra(width, height, stride, bytes, frame_from_bgra);
    }

    /// Ends the stream: the videos playing it show nothing from now on,
    /// until the name is registered again.
    pub fn end(&self) {
        self.0.end();
    }
}

/// The GPUI image of `width` × `height` BGRA pixels in rows of `stride`
/// bytes (checked by the core already), rows packed; `None` when the size
/// does not fit a GPUI image.
fn frame_from_bgra(width: usize, height: usize, stride: usize, bytes: &[u8]) -> Option<Frame> {
    let row = width.checked_mul(4)?;
    if width == 0 || height == 0 || stride < row || bytes.len() < stride.checked_mul(height)? {
        return None;
    }
    let (w, h) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    let mut pixels = Vec::with_capacity(row.checked_mul(height)?);
    for y in 0..height {
        pixels.extend_from_slice(&bytes[y * stride..y * stride + row]);
    }
    let image = image::RgbaImage::from_raw(w, h, pixels)?;
    Some(Frame {
        width: w,
        height: h,
        image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
    })
}

/// A video's attachment to a stream, by name ([`Streams::attach`]);
/// dropping it detaches the video's sink. Main thread.
pub(crate) struct Attached(nsh_core::stream::Attached<Frame>);

impl Attached {
    /// The stream's name.
    pub(crate) fn name(&self) -> &Arc<str> {
        self.0.name()
    }

    /// Stops or resumes delivering frames to the video's sink (the last
    /// one stays; a video without one yet still takes the first).
    pub(crate) fn set_paused(&self, paused: bool) {
        self.0.set_paused(paused);
    }
}

/// The streams of one view's registry ([`crate::Components`]), by name
/// (shared with its videos, so a registration applies to videos showing
/// the name from then on, and to those already showing it). Main thread.
#[derive(Clone)]
pub(crate) struct Streams(nsh_core::stream::Streams<Frame>);

impl Default for Streams {
    fn default() -> Self {
        Streams(nsh_core::stream::Streams::new(
            |job: Box<dyn FnOnce() + Send>| crate::rt::invoke(job),
        ))
    }
}

impl std::fmt::Debug for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
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
    /// and the end, on the main thread. A name nothing registered is logged
    /// once; the sink hears frames once something is.
    pub(crate) fn attach(&self, name: &str, sink: Box<dyn Fn(StreamEvent<'_>)>) -> Attached {
        Attached(self.0.attach(name, sink))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, stride: usize, bgra: [u8; 4]) -> Vec<u8> {
        let mut bytes = vec![0u8; stride * height];
        for y in 0..height {
            for px in bytes[y * stride..y * stride + width * 4]
                .as_chunks_mut::<4>()
                .0
            {
                px.copy_from_slice(&bgra);
            }
        }
        bytes
    }

    #[test]
    fn a_frame_drops_its_row_padding() {
        // The rows are padded (a stride wider than the row), which the
        // frame drops: the image is width × height, every pixel the color.
        let frame = frame_from_bgra(2, 2, 12, &solid(2, 2, 12, [4, 5, 6, 255])).expect("a frame");
        assert_eq!((frame.width, frame.height), (2, 2));
        let bytes = frame.image.as_bytes(0).expect("one frame");
        assert_eq!(bytes.len(), 16);
        for px in bytes.as_chunks::<4>().0 {
            assert_eq!(px, &[4, 5, 6, 255]);
        }
    }

    #[test]
    fn a_frame_refuses_a_wrong_size() {
        // Too few bytes, a stride narrower than a row, no size.
        assert!(frame_from_bgra(2, 2, 8, &[0; 12]).is_none());
        assert!(frame_from_bgra(2, 2, 4, &[0; 32]).is_none());
        assert!(frame_from_bgra(0, 2, 8, &[0; 32]).is_none());
        assert!(frame_from_bgra(2, 2, 8, &[0; 16]).is_some());
    }

    #[test]
    fn one_entry_per_name() {
        let streams = Streams::default();
        let _a = streams.attach("x", Box::new(|_| {}));
        let h = streams.register("x");
        assert_eq!(h.name(), "x");
        assert_eq!(format!("{h:?}"), r#"StreamHandle { name: "x" }"#);
        assert_eq!(format!("{streams:?}"), r#"{"x"}"#);
    }
}
