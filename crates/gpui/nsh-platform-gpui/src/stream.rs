//! Live streams the embedding app registers by name, which `<Video
//! srcObject>` plays (`MediaStream.fromHost(name)`, `VideoProps::stream`):
//! the host pushes BGRA frames from any thread, and each video showing the
//! stream paints them as its picture, on the main thread. The frames never
//! pass through JS.
//!
//! A frame becomes a GPUI image ([`RenderImage`]) on the pushing thread,
//! once, however many videos show it; the videos hear it through the
//! sinks they attached ([`Streams::attach`]), on the main thread, one hop
//! ([`crate::rt::invoke`]) per batch of frames: a frame pushed before the
//! previous one was delivered replaces it, so a slow main thread drops
//! frames rather than queueing them.
//!
//! A name is one entry ([`Shared`]) whether the host registered it or a
//! video asked for it first: a video attaches its sink to the entry, and
//! the frames of a registration that comes later reach it. A name nothing
//! registered is logged once, as an unknown native component is.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

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
pub(crate) enum StreamEvent<'a> {
    /// A new frame to show.
    Frame(&'a Frame),
    /// The registration ended: show nothing until another frame comes.
    Ended,
}

/// Hears a stream's events, on the main thread. `Send + Sync` so that the
/// sinks may live in the entry other threads push to: a video's sink
/// finds the video again by surface key and node id, as the player's
/// wakes do.
pub(crate) type Sink = Box<dyn Fn(StreamEvent<'_>) + Send + Sync>;

/// A video showing a stream: its sink and what it hears.
struct Attachment {
    key: u64,
    /// `paused`: frames are dropped, the last one stays. A paused video
    /// without a frame yet still takes the first that comes, as a paused
    /// `<video>` shows its first frame.
    paused: AtomicBool,
    /// A frame was delivered since the registration started.
    loaded: AtomicBool,
    sink: Sink,
}

/// One stream by name, shared by its registration and the videos showing
/// it. Frames come from any thread; the sinks hear them on the main
/// thread, one hop per batch of frames.
struct Shared {
    name: Arc<str>,
    /// The frame waiting to be delivered.
    pending: Mutex<Option<Frame>>,
    /// A delivery is queued on the main thread for `pending`.
    scheduled: AtomicBool,
    /// Whether a registration is live (frames may come).
    registered: AtomicBool,
    /// A wrong push was reported (once per stream).
    warned: AtomicBool,
    /// The videos showing the stream. Taken as a snapshot to deliver, so a
    /// sink may attach or detach while it runs.
    sinks: Mutex<Vec<Arc<Attachment>>>,
}

impl Shared {
    fn new(name: Arc<str>) -> Arc<Self> {
        Arc::new(Shared {
            name,
            pending: Mutex::new(None),
            scheduled: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            sinks: Mutex::new(Vec::new()),
        })
    }

    fn attachments(&self) -> Vec<Arc<Attachment>> {
        self.sinks.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Queues `frame` and a delivery of it on the main thread (one at a
    /// time).
    fn push(self: &Arc<Self>, frame: Frame) {
        if !self.registered.load(Ordering::Acquire) {
            return;
        }
        // No video shows the stream: nothing to hop for (a host pushing
        // from before the app runs would log at every frame).
        if self
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return;
        }
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = self.clone();
        // A frame pushed before the app runs (a host registering at start)
        // waits for the next push rather than wedging the stream.
        if !crate::rt::invoke(move || shared.deliver()) {
            self.scheduled.store(false, Ordering::Release);
        }
    }

    /// On the main thread: hands the pending frame to every sink that is
    /// not paused.
    fn deliver(&self) {
        self.scheduled.store(false, Ordering::Release);
        let frame = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(frame) = frame else { return };
        for attachment in self.attachments() {
            if !attachment.paused.load(Ordering::Acquire)
                || !attachment.loaded.load(Ordering::Acquire)
            {
                attachment.loaded.store(true, Ordering::Release);
                (attachment.sink)(StreamEvent::Frame(&frame));
            }
        }
    }

    /// The registration ended: the videos show nothing until another.
    fn end(self: &Arc<Self>) {
        self.registered.store(false, Ordering::Release);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let shared = self.clone();
        crate::rt::invoke(move || {
            for attachment in shared.attachments() {
                attachment.loaded.store(false, Ordering::Release);
                (attachment.sink)(StreamEvent::Ended);
            }
        });
    }
}

/// A stream the embedding app registered ([`Streams::register`],
/// [`crate::AppView::register_stream`]): where it pushes the frames. Any
/// thread. Dropping it ends the stream.
pub struct StreamHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle")
            .field("name", &self.shared.name)
            .finish()
    }
}

impl StreamHandle {
    /// The name it was registered as.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Shows a frame in the videos playing the stream: `width` × `height`
    /// pixels of BGRA (8 bits each, opaque or premultiplied), `stride`
    /// bytes per row (at least `width * 4`). Copies the bytes. A frame
    /// that comes before the previous one was drawn replaces it. A frame
    /// that does not fit its size (or has none) is a wrong use: it shows
    /// nothing, logged once per stream.
    pub fn push_bgra(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) {
        let Some(frame) = frame_from_bgra(width, height, stride, bytes) else {
            if !self.shared.warned.swap(true, Ordering::AcqRel) {
                log::warn!(
                    "stream `{}`: a frame of {width}×{height} pixels with {stride} bytes per \
                     row does not fit {} bytes: it shows nothing",
                    self.shared.name,
                    bytes.len()
                );
            }
            return;
        };
        self.shared.push(frame);
    }

    /// Ends the stream: the videos playing it show nothing from now on,
    /// until the name is registered again.
    pub fn end(&self) {
        self.shared.end();
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.shared.end();
    }
}

/// The GPUI image of `width` × `height` BGRA pixels in rows of `stride`
/// bytes, rows packed; `None` when they do not fit.
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
/// dropping it detaches the video's sink.
pub(crate) struct Attached {
    shared: Arc<Shared>,
    attachment: Arc<Attachment>,
}

impl Attached {
    /// The stream's name.
    pub(crate) fn name(&self) -> &Arc<str> {
        &self.shared.name
    }

    /// Stops or resumes delivering frames to the video's sink (the last
    /// one stays; a video without one yet still takes the first).
    pub(crate) fn set_paused(&self, paused: bool) {
        self.attachment.paused.store(paused, Ordering::Release);
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let key = self.attachment.key;
        self.shared
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|a| a.key != key);
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// The streams of one view's registry ([`crate::Components`]), by name
/// (shared with its videos, so a registration applies to videos showing
/// the name from then on, and to those already showing it). Main thread.
#[derive(Clone, Default)]
pub(crate) struct Streams {
    entries: Rc<RefCell<HashMap<Arc<str>, Arc<Shared>>>>,
    /// Names already reported as unregistered.
    missing: Rc<RefCell<HashSet<Arc<str>>>>,
}

impl std::fmt::Debug for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.entries.borrow().keys()).finish()
    }
}

impl Streams {
    /// The entry of `name`, made if none.
    fn entry(&self, name: &str) -> Arc<Shared> {
        let mut entries = self.entries.borrow_mut();
        if let Some(shared) = entries.get(name) {
            return shared.clone();
        }
        let name: Arc<str> = name.into();
        let shared = Shared::new(name.clone());
        entries.insert(name, shared.clone());
        shared
    }

    /// Registers stream `name` (replacing an earlier registration): the
    /// handle takes its frames. The videos whose `srcObject` names it,
    /// now or later, play it.
    pub(crate) fn register(&self, name: &str) -> StreamHandle {
        let shared = self.entry(name);
        shared.registered.store(true, Ordering::Release);
        self.missing.borrow_mut().remove(name);
        StreamHandle { shared }
    }

    /// Attaches `sink`, a video's, to stream `name`: it hears the frames
    /// and the end, on the main thread. A name nothing registered is logged
    /// once; the sink hears frames once something is.
    pub(crate) fn attach(&self, name: &str, sink: Sink) -> Attached {
        let shared = self.entry(name);
        if !shared.registered.load(Ordering::Acquire)
            && self.missing.borrow_mut().insert(shared.name.clone())
        {
            log::warn!(
                target: "js",
                "no stream is registered as `{name}`: the video shows nothing"
            );
        }
        let attachment = Arc::new(Attachment {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            paused: AtomicBool::new(false),
            loaded: AtomicBool::new(false),
            sink,
        });
        shared
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(attachment.clone());
        Attached { shared, attachment }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// What a sink heard: a frame's size and its first pixel, or `None`
    /// for the end.
    type Heard = Option<(u32, u32, [u8; 4])>;

    /// Records what a sink hears.
    fn recording() -> (Sink, mpsc::Receiver<Heard>) {
        let (tx, rx) = mpsc::channel();
        let sink: Sink = Box::new(move |event| {
            let heard = match event {
                StreamEvent::Frame(frame) => {
                    let bytes = frame.image.as_bytes(0).expect("one frame");
                    Some((
                        frame.width,
                        frame.height,
                        [bytes[0], bytes[1], bytes[2], bytes[3]],
                    ))
                }
                StreamEvent::Ended => None,
            };
            let _ = tx.send(heard);
        });
        (sink, rx)
    }

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
    fn newest_frame_wins_and_padding_is_dropped() {
        let streams = Streams::default();
        let (sink, rx) = recording();
        // Attached first: the registration that follows reaches it.
        let attached = streams.attach("cam", sink);
        let handle = streams.register("cam");
        assert_eq!(handle.name(), "cam");
        // Two frames before a delivery: only the second is seen. The rows
        // are padded (a stride wider than the row), which the frame drops.
        handle.push_bgra(2, 2, 12, &solid(2, 2, 12, [1, 2, 3, 255]));
        handle.push_bgra(2, 2, 12, &solid(2, 2, 12, [4, 5, 6, 255]));
        let shared = streams.entry("cam");
        shared.deliver();
        assert_eq!(rx.try_recv(), Ok(Some((2, 2, [4, 5, 6, 255]))));
        assert!(rx.try_recv().is_err());
        assert_eq!(shared.attachments().len(), 1);
        drop(attached);
        assert!(shared.attachments().is_empty());
    }

    #[test]
    fn paused_sinks_keep_the_last_frame_and_the_end_reaches_all() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        let (sink, rx) = recording();
        let attached = streams.attach("cam", sink);
        let shared = streams.entry("cam");
        // Paused before any frame: the first one still shows (a paused
        // video shows its first frame), the next is dropped.
        attached.set_paused(true);
        handle.push_bgra(1, 1, 4, &[7, 7, 7, 255]);
        shared.deliver();
        assert_eq!(rx.try_recv(), Ok(Some((1, 1, [7, 7, 7, 255]))));
        handle.push_bgra(1, 1, 4, &[8, 8, 8, 255]);
        shared.deliver();
        assert!(rx.try_recv().is_err(), "a paused sink hears no later frame");
        attached.set_paused(false);
        handle.push_bgra(1, 1, 4, &[9, 9, 9, 255]);
        shared.deliver();
        assert_eq!(rx.try_recv(), Ok(Some((1, 1, [9, 9, 9, 255]))));
        // Ended: pending frames go, pushes after it show nothing.
        handle.push_bgra(1, 1, 4, &[1, 1, 1, 255]);
        handle.end();
        shared.deliver();
        assert!(rx.try_recv().is_err());
        for attachment in shared.attachments() {
            attachment.loaded.store(false, Ordering::Release);
            (attachment.sink)(StreamEvent::Ended);
        }
        assert_eq!(rx.try_recv(), Ok(None));
        handle.push_bgra(1, 1, 4, &[1, 1, 1, 255]);
        assert!(shared.pending.lock().unwrap().is_none());
        // Registered again: frames flow.
        let again = streams.register("cam");
        again.push_bgra(1, 1, 4, &[2, 2, 2, 255]);
        shared.deliver();
        assert_eq!(rx.try_recv(), Ok(Some((1, 1, [2, 2, 2, 255]))));
    }

    #[test]
    fn a_wrong_push_shows_nothing_and_does_not_panic() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        let shared = streams.entry("cam");
        // A video shows it: frames are kept for the main thread.
        let (sink, _heard) = recording();
        let _attached = streams.attach("cam", sink);
        // Too few bytes, a stride narrower than a row, no size.
        handle.push_bgra(2, 2, 8, &[0; 12]);
        handle.push_bgra(2, 2, 4, &[0; 32]);
        handle.push_bgra(0, 2, 8, &[0; 32]);
        assert!(shared.pending.lock().unwrap().is_none());
        assert!(shared.warned.load(Ordering::Acquire));
        // A right one after them still shows.
        handle.push_bgra(2, 2, 8, &[0; 32]);
        assert!(shared.pending.lock().unwrap().is_some());
    }

    #[test]
    fn one_entry_per_name() {
        let streams = Streams::default();
        let (sink, _rx) = recording();
        let _a = streams.attach("x", sink);
        let _h = streams.register("x");
        assert_eq!(streams.entries.borrow().len(), 1);
        assert_eq!(format!("{streams:?}"), r#"{"x"}"#);
    }
}
