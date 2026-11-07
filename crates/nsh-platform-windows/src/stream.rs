//! Live streams the embedding app registers by name, which `<Video
//! srcObject>` plays (`MediaStream.fromHost(name)`, `VideoProps::stream`):
//! the host pushes frames (BGRA bytes) from any thread, and each video
//! showing the stream copies them into a Direct2D bitmap of its own on the
//! UI thread ([`crate::native`]'s `video`). The frames never pass through
//! JS.
//!
//! A name is one entry ([`Shared`]) whether the host registered it or a
//! video asked for it first: a video attaches its sink to the entry, and
//! the frames of a registration that comes later reach it. A name nothing
//! registered is logged once, as an unknown native component is.
//!
//! The Apple backends' `nsh_platform_apple::stream` is the model: the same
//! contract with `CVPixelBuffer`s and a display layer per video.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use rustc_hash::{FxHashMap, FxHashSet};

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
pub(crate) enum StreamEvent<'a> {
    /// A frame to show.
    Frame(&'a Frame),
    /// The registration ended: show nothing until another frame.
    Ended,
}

/// A video showing a stream: what it hears and whether it does.
struct Sink {
    key: u64,
    /// `paused`: frames are dropped, the last one shown stays; a paused
    /// video with no frame yet still draws the first that comes, as a
    /// paused `<video>` shows its first frame.
    paused: AtomicBool,
    /// A frame was delivered since the stream started or last ended.
    loaded: AtomicBool,
    /// Hears the frames and the end, on the UI thread of the surface that
    /// attached it (the only thread that calls it: the registry hops there
    /// before delivering).
    sink: Box<dyn Fn(StreamEvent<'_>) + Send>,
}

/// One stream by name, shared by its registration and the videos showing
/// it. Frames come from any thread; the sinks hear them on the UI thread,
/// one hop per batch of frames (a frame pushed before the previous one was
/// delivered replaces it: a live stream shows the newest).
pub(crate) struct Shared {
    name: Arc<str>,
    /// The frame waiting to be delivered.
    pending: Mutex<Option<Frame>>,
    /// A delivery is queued on the UI thread for `pending`.
    scheduled: AtomicBool,
    /// Whether a registration is live (frames may come).
    registered: AtomicBool,
    /// A malformed frame was reported (once per stream).
    warned: AtomicBool,
    /// The UI thread the sinks live on: the thread that attached the first
    /// one (`None` until a video attaches; the registry is one thread's).
    ui: Mutex<Option<UiThread>>,
    sinks: Mutex<Vec<Arc<Sink>>>,
}

/// Resets `scheduled` when the queued delivery is done with, run or not
/// (the UI thread may be gone and drop the closure unrun: the next frame
/// must still queue one).
struct Scheduled(Arc<Shared>);

impl Drop for Scheduled {
    fn drop(&mut self) {
        self.0.scheduled.store(false, Ordering::Release);
    }
}

impl Shared {
    fn new(name: Arc<str>) -> Arc<Self> {
        Arc::new(Shared {
            name,
            pending: Mutex::new(None),
            scheduled: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            ui: Mutex::new(None),
            sinks: Mutex::new(Vec::new()),
        })
    }

    fn ui(&self) -> Option<UiThread> {
        *self.ui.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `frame` and a delivery of it on the UI thread (one at a
    /// time). Nothing is queued before a video attaches: there is no one to
    /// deliver to.
    fn push(self: &Arc<Self>, frame: Frame) {
        if !self.registered.load(Ordering::Acquire) {
            return;
        }
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let scheduled = Scheduled(self.clone());
        let Some(ui) = self.ui() else {
            return;
        };
        ui.invoke(move || {
            let shared = scheduled.0.clone();
            drop(scheduled);
            let frame = shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(frame) = frame {
                shared.deliver(StreamEvent::Frame(&frame));
            }
        });
    }

    /// Hands `event` to every sink (a frame only to those not paused, or
    /// paused with no frame yet). The sinks are copied out first, so one
    /// may attach, detach or pause from inside without deadlocking.
    fn deliver(&self, event: StreamEvent<'_>) {
        let sinks: Vec<Arc<Sink>> = self.sinks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for sink in &sinks {
            match event {
                StreamEvent::Frame(frame) => {
                    if !sink.paused.load(Ordering::Relaxed) || !sink.loaded.load(Ordering::Relaxed)
                    {
                        (sink.sink)(StreamEvent::Frame(frame));
                        sink.loaded.store(true, Ordering::Relaxed);
                    }
                }
                StreamEvent::Ended => {
                    (sink.sink)(StreamEvent::Ended);
                    sink.loaded.store(false, Ordering::Relaxed);
                }
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
        let Some(ui) = self.ui() else {
            return;
        };
        let shared = self.clone();
        ui.invoke(move || shared.deliver(StreamEvent::Ended));
    }
}

/// A stream the embedding app registered ([`crate::AppView::register_stream`]):
/// where it pushes the frames. Any thread. Dropping it ends the stream.
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
    /// that does not add up (`bytes` shorter than `stride * height`, a
    /// stride under `width * 4`, an empty size) is logged once and shows
    /// nothing.
    pub fn push_bgra(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) {
        if let Err(what) = check_frame(width, height, stride, bytes.len()) {
            if !self.shared.warned.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "stream `{}`: a frame of {width}×{height} with {} bytes per row and {} bytes in all is dropped: {what}",
                    self.shared.name,
                    stride,
                    bytes.len()
                );
            }
            return;
        }
        self.shared.push(Frame {
            width,
            height,
            stride,
            bytes: bytes[..stride * height].to_vec(),
        });
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

/// Why a frame of `width` × `height` pixels, `stride` bytes per row and
/// `len` bytes in all cannot be shown, if it cannot.
fn check_frame(width: usize, height: usize, stride: usize, len: usize) -> Result<(), &'static str> {
    if width == 0 || height == 0 {
        return Err("the size is empty");
    }
    if width.checked_mul(4).is_none_or(|row| stride < row) {
        return Err("the stride is shorter than a row of BGRA pixels");
    }
    if stride.checked_mul(height).is_none_or(|all| len < all) {
        return Err("the bytes are fewer than stride × height");
    }
    Ok(())
}

/// A video's attachment to a stream, by name ([`Streams::attach`]);
/// dropping it detaches the video's sink.
pub(crate) struct Attached {
    shared: Arc<Shared>,
    key: u64,
}

impl Attached {
    /// The stream's name.
    pub(crate) fn name(&self) -> &str {
        &self.shared.name
    }

    /// Stops or resumes delivering frames to the video's sink (the last
    /// one it drew stays; with none drawn yet, the first to come is).
    pub(crate) fn set_paused(&self, paused: bool) {
        for sink in self
            .shared
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            if sink.key == self.key {
                sink.paused.store(paused, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.shared
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|s| s.key != self.key);
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// The streams of one registry ([`crate::Components`]), by name (shared
/// with its videos, so a registration applies to videos showing the name
/// from then on, and to those already showing it). One thread's: the UI
/// thread of the surfaces made with the registry.
#[derive(Clone, Default)]
pub(crate) struct Streams {
    entries: Rc<RefCell<FxHashMap<Arc<str>, Arc<Shared>>>>,
    /// Names already reported as unregistered.
    missing: Rc<RefCell<FxHashSet<Arc<str>>>>,
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
    /// handle takes its frames. The videos whose `srcObject` names it, now
    /// or later, play it.
    pub(crate) fn register(&self, name: &str) -> StreamHandle {
        let shared = self.entry(name);
        shared.registered.store(true, Ordering::Release);
        self.missing.borrow_mut().remove(name);
        StreamHandle { shared }
    }

    /// Attaches `sink`, a video's, to stream `name`: it hears the frames
    /// and the end on this thread, the UI thread of the video's surface. A
    /// name nothing registered is logged once; the sink hears once
    /// something is.
    pub(crate) fn attach(&self, name: &str, sink: Box<dyn Fn(StreamEvent<'_>) + Send>) -> Attached {
        let shared = self.entry(name);
        if !shared.registered.load(Ordering::Acquire)
            && self.missing.borrow_mut().insert(shared.name.clone())
        {
            log::warn!(
                target: "js",
                "no stream is registered as `{name}`: the video shows nothing"
            );
        }
        shared
            .ui
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(UiThread::current);
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        shared
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(Sink {
                key,
                paused: AtomicBool::new(false),
                loaded: AtomicBool::new(false),
                sink,
            }));
        Attached { shared, key }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_frames() {
        assert_eq!(check_frame(2, 2, 8, 16), Ok(()));
        // A stride with padding, and more bytes than needed.
        assert_eq!(check_frame(2, 2, 12, 30), Ok(()));
        assert!(check_frame(0, 2, 8, 16).is_err());
        assert!(check_frame(2, 0, 8, 16).is_err());
        assert!(check_frame(2, 2, 7, 16).is_err());
        assert!(check_frame(2, 2, 8, 15).is_err());
        assert!(check_frame(usize::MAX, 1, usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn a_name_is_one_entry_whichever_side_comes_first() {
        let streams = Streams::default();
        let attached = streams.attach("cam", Box::new(|_| {}));
        assert_eq!(attached.name(), "cam");
        let handle = streams.register("cam");
        assert_eq!(handle.name(), "cam");
        assert!(Arc::ptr_eq(&attached.shared, &handle.shared));
        assert_eq!(streams.entries.borrow().len(), 1);
        assert!(handle.shared.registered.load(Ordering::Acquire));
        // The sink is there until the attachment drops.
        assert_eq!(handle.shared.sinks.lock().unwrap().len(), 1);
        drop(attached);
        assert_eq!(handle.shared.sinks.lock().unwrap().len(), 0);
    }

    #[test]
    fn frames_wait_for_the_ui_thread_and_the_newest_wins() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        let shared = handle.shared.clone();
        // No video yet: the frame is kept, nothing is queued.
        handle.push_bgra(1, 1, 4, &[1, 2, 3, 255]);
        assert!(shared.pending.lock().unwrap().is_some());
        assert!(!shared.scheduled.load(Ordering::Acquire));
        let _attached = streams.attach("cam", Box::new(|_| {}));
        handle.push_bgra(1, 1, 4, &[4, 5, 6, 255]);
        handle.push_bgra(1, 1, 4, &[7, 8, 9, 255]);
        // One delivery queued (never run here: no message loop), holding
        // the newest frame.
        assert!(shared.scheduled.load(Ordering::Acquire));
        assert_eq!(
            shared
                .pending
                .lock()
                .unwrap()
                .as_ref()
                .map(|f| &f.bytes[..]),
            Some(&[7, 8, 9, 255][..])
        );
        // Ending drops it and stops the handle's frames.
        handle.end();
        assert!(shared.pending.lock().unwrap().is_none());
        handle.push_bgra(1, 1, 4, &[1, 1, 1, 255]);
        assert!(shared.pending.lock().unwrap().is_none());
        assert!(!shared.registered.load(Ordering::Acquire));
    }

    #[test]
    fn a_paused_sink_draws_its_first_frame_only() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        let heard = Arc::new(Mutex::new(Vec::new()));
        let seen = heard.clone();
        let attached = streams.attach(
            "cam",
            Box::new(move |event| {
                seen.lock().unwrap().push(match event {
                    StreamEvent::Frame(f) => f.bytes[0],
                    StreamEvent::Ended => 0,
                });
            }),
        );
        attached.set_paused(true);
        let frame = |b: u8| Frame {
            width: 1,
            height: 1,
            stride: 4,
            bytes: vec![b, 0, 0, 255],
        };
        // Delivered here directly: the UI thread's hop needs a message loop.
        handle.shared.deliver(StreamEvent::Frame(&frame(1)));
        handle.shared.deliver(StreamEvent::Frame(&frame(2)));
        attached.set_paused(false);
        handle.shared.deliver(StreamEvent::Frame(&frame(3)));
        attached.set_paused(true);
        handle.shared.deliver(StreamEvent::Ended);
        // After the end, the next frame is a first one again.
        handle.shared.deliver(StreamEvent::Frame(&frame(4)));
        handle.shared.deliver(StreamEvent::Frame(&frame(5)));
        assert_eq!(*heard.lock().unwrap(), vec![1, 3, 0, 4]);
    }

    #[test]
    fn malformed_frames_are_dropped() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        let _attached = streams.attach("cam", Box::new(|_| {}));
        handle.push_bgra(2, 2, 8, &[0; 15]);
        handle.push_bgra(0, 2, 8, &[0; 16]);
        assert!(handle.shared.pending.lock().unwrap().is_none());
        assert!(handle.shared.warned.load(Ordering::Relaxed));
    }

    #[test]
    fn dropping_the_handle_ends_the_stream() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        let shared = handle.shared.clone();
        drop(handle);
        assert!(!shared.registered.load(Ordering::Acquire));
        // Registering again revives the entry.
        let handle = streams.register("cam");
        assert!(Arc::ptr_eq(&shared, &handle.shared));
        assert!(shared.registered.load(Ordering::Acquire));
    }
}
