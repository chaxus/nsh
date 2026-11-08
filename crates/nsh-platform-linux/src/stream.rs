//! Live streams the embedding app registers by name, which `<Video
//! srcObject>` plays (`MediaStream.fromHost(name)`, `VideoProps::stream`):
//! the host pushes frames (BGRA bytes) from any thread, and each video
//! showing the stream draws them as a `gdk::MemoryTexture` in its
//! `gtk::Picture`, on the main thread. The frames never pass through JS.
//!
//! A name is one entry ([`Shared`]) whether the host registered it or a
//! video asked for it first: a video attaches its sink to the entry, and
//! the frames of a registration that comes later reach it. A name nothing
//! registered is logged once, as an unknown native component is.
//!
//! The sinks (a video's picture and emitter) belong to the main thread, so
//! they live in a thread-local of it keyed by the stream, as the surfaces
//! do (`surface.rs`): the main-thread hop finds them there, and a sink of
//! another thread (a test's) is simply not found.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

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
    /// A frame of `bytes`, copied; `None` for a size no frame can have.
    fn new(width: usize, height: usize, stride: usize, bytes: &[u8]) -> Option<Self> {
        let rows = stride.checked_mul(height)?;
        if width == 0 || height == 0 || stride < width.checked_mul(4)? || bytes.len() < rows {
            return None;
        }
        Some(Frame {
            width,
            height,
            stride,
            bytes: glib::Bytes::from(&bytes[..rows]),
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
pub(crate) enum StreamEvent<'a> {
    /// A frame to show.
    Frame(&'a Frame),
    /// The registration ended: the video shows nothing until another.
    Ended,
}

/// A video showing a stream: what it hears.
struct Sink {
    key: u64,
    sink: Box<dyn Fn(StreamEvent<'_>)>,
    /// `paused`: frames are dropped, the last one stays; a sink with no
    /// frame yet still gets the first one, as a paused `<video>` shows its
    /// first frame.
    paused: Cell<bool>,
    /// Whether it got a frame since it attached or the stream ended.
    loaded: Cell<bool>,
}

thread_local! {
    /// The main thread's sinks, by stream key.
    static SINKS: RefCell<HashMap<u64, Vec<Rc<Sink>>>> = RefCell::default();
}

/// What waits for the main thread.
#[derive(Default)]
struct Pending {
    /// The frame waiting to be drawn.
    frame: Option<Frame>,
    /// The registration ended since the last draw.
    ended: bool,
}

/// One stream by name, shared by its registration and the videos showing
/// it. Frames come from any thread; the sinks are told on the main thread,
/// one hop per batch of frames (a frame pushed before the previous one was
/// drawn replaces it: a live stream shows the newest).
pub(crate) struct Shared {
    name: Arc<str>,
    /// Its sinks' key in [`SINKS`].
    key: u64,
    pending: Mutex<Pending>,
    /// A draw is queued on the main thread for `pending`.
    scheduled: AtomicBool,
    /// Whether a registration is live (frames may come).
    registered: AtomicBool,
    /// A frame whose bytes did not fit its size was reported (once).
    warned: AtomicBool,
    /// How many videos show the stream (its sinks' count, readable off
    /// the main thread): with none, a frame waits without a hop.
    attached: AtomicUsize,
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

impl Shared {
    fn new(name: Arc<str>) -> Arc<Self> {
        Arc::new(Shared {
            name,
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            pending: Mutex::default(),
            scheduled: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            attached: AtomicUsize::new(0),
        })
    }

    /// Whether frames are wanted: a registration is live and a video shows
    /// the stream (with none, a host pushing at 30 fps costs nothing).
    fn wanted(&self) -> bool {
        self.registered.load(Ordering::Acquire) && self.attached.load(Ordering::Acquire) > 0
    }

    /// Queues `frame` and a draw of it on the main thread (one at a time).
    fn push(self: &Arc<Self>, frame: Frame) {
        if !self.wanted() {
            return;
        }
        self.store(frame);
        self.schedule();
    }

    /// Keeps `frame` for the next draw, in place of one not drawn yet.
    fn store(&self, frame: Frame) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).frame = Some(frame);
    }

    /// The registration ended: the videos show nothing until another.
    fn end(self: &Arc<Self>) {
        self.registered.store(false, Ordering::Release);
        self.mark_ended();
        self.schedule();
    }

    /// Keeps the end for the next draw, dropping a frame not drawn yet.
    fn mark_ended(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.frame = None;
        pending.ended = true;
    }

    /// One hop to the main thread for what is pending, unless one is on
    /// its way.
    fn schedule(self: &Arc<Self>) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = self.clone();
        glib::MainContext::default().invoke(move || shared.drain());
    }

    /// Tells the sinks what is pending (main thread): the end first, then
    /// the frame to every sink that is not paused.
    fn drain(&self) {
        self.scheduled.store(false, Ordering::Release);
        let Pending { frame, ended } =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        // Cloned out, so a sink that attaches or detaches while told does
        // not meet the borrow.
        let sinks: Vec<Rc<Sink>> =
            SINKS.with(|s| s.borrow().get(&self.key).cloned().unwrap_or_default());
        if ended {
            for sink in &sinks {
                sink.loaded.set(false);
                (sink.sink)(StreamEvent::Ended);
            }
        }
        if let Some(frame) = &frame {
            for sink in sinks.iter().filter(|s| !s.paused.get() || !s.loaded.get()) {
                sink.loaded.set(true);
                (sink.sink)(StreamEvent::Frame(frame));
            }
        }
    }
}

/// A stream the embedding app registered ([`Streams::register`]): where it
/// pushes the frames. Any thread. Dropping it ends the stream.
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
    /// bytes per row (at least `width * 4`). Copies the bytes. A frame that
    /// comes before the previous one was drawn replaces it. A frame of a
    /// size no frame can have (zero, a stride shorter than its rows, fewer
    /// bytes than its rows) shows nothing, logged once per stream.
    pub fn push_bgra(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) {
        // No video shows the stream: the bytes are not even copied.
        if !self.shared.wanted() {
            return;
        }
        if let Some(frame) = self.frame(width, height, stride, bytes) {
            self.shared.push(frame);
        }
    }

    /// Ends the stream: the videos playing it show nothing from now on,
    /// until the name is registered again.
    pub fn end(&self) {
        self.shared.end();
    }

    /// The frame of `bytes`, or `None` (logged once) for a size no frame
    /// can have.
    fn frame(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) -> Option<Frame> {
        let frame = Frame::new(width, height, stride, bytes);
        if frame.is_none() && !self.shared.warned.swap(true, Ordering::Relaxed) {
            log::warn!(
                "stream `{}`: a frame of {width}×{height} pixels with a stride of {stride} bytes \
                 cannot have {} bytes: it shows nothing",
                self.shared.name,
                bytes.len()
            );
        }
        frame
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.shared.end();
    }
}

/// A video's attachment to a stream, by name ([`Streams::attach`]);
/// dropping it detaches the video's sink. Main thread.
pub(crate) struct Attached {
    shared: Arc<Shared>,
    key: u64,
}

impl Attached {
    /// The stream's name.
    pub(crate) fn name(&self) -> &Arc<str> {
        &self.shared.name
    }

    /// Stops or resumes telling the sink the frames (the last one stays; a
    /// sink with none yet still gets the first).
    pub(crate) fn set_paused(&self, paused: bool) {
        SINKS.with(|s| {
            if let Some(sinks) = s.borrow().get(&self.shared.key) {
                for sink in sinks.iter().filter(|s| s.key == self.key) {
                    sink.paused.set(paused);
                }
            }
        });
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        SINKS.with(|s| {
            let mut sinks = s.borrow_mut();
            if let Some(entry) = sinks.get_mut(&self.shared.key) {
                entry.retain(|s| s.key != self.key);
                self.shared.attached.store(entry.len(), Ordering::Release);
                if entry.is_empty() {
                    sinks.remove(&self.shared.key);
                }
            }
        });
    }
}

/// The streams of one surface, by name (shared with its videos, so a
/// registration applies to videos showing the name from then on, and to
/// those already showing it). Main thread.
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
    /// and the end. A name nothing registered is logged once; the sink
    /// hears frames once something is.
    pub(crate) fn attach(&self, name: &str, sink: Box<dyn Fn(StreamEvent<'_>)>) -> Attached {
        let shared = self.entry(name);
        if !shared.registered.load(Ordering::Acquire)
            && self.missing.borrow_mut().insert(shared.name.clone())
        {
            log::warn!(
                target: "js",
                "no stream is registered as `{name}`: the video shows nothing"
            );
        }
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        SINKS.with(|s| {
            let mut sinks = s.borrow_mut();
            let entry = sinks.entry(shared.key).or_default();
            entry.push(Rc::new(Sink {
                key,
                sink,
                paused: Cell::new(false),
                loaded: Cell::new(false),
            }));
            shared.attached.store(entry.len(), Ordering::Release);
        });
        // A frame pushed but not drawn yet shows in the new video at once.
        if shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .frame
            .is_some()
        {
            shared.schedule();
        }
        Attached { shared, key }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records what it hears: frame sizes, or `end`.
    fn recorder() -> (Box<dyn Fn(StreamEvent<'_>)>, Rc<RefCell<Vec<String>>>) {
        let heard = Rc::new(RefCell::new(Vec::new()));
        let log = heard.clone();
        let sink = Box::new(move |event: StreamEvent<'_>| {
            log.borrow_mut().push(match event {
                StreamEvent::Frame(f) => format!("{}x{}", f.width, f.height),
                StreamEvent::Ended => "end".to_owned(),
            });
        });
        (sink, heard)
    }

    // The main-thread hop (`schedule`) goes through GLib's default main
    // context, which another test's GTK thread may own: the tests tell the
    // sinks what is pending themselves (`drain`), on their own thread.

    #[test]
    fn frames_reach_the_sinks_newest_first_and_paused_ones_keep_the_last() {
        let streams = Streams::default();
        let (sink, heard) = recorder();
        let attached = streams.attach("cam", sink);
        let handle = streams.register("cam");
        assert_eq!(handle.name(), "cam");
        // A registration is the entry the video attached to.
        assert!(Arc::ptr_eq(&attached.shared, &handle.shared));

        let bytes = vec![0u8; 8 * 2];
        handle.shared.store(handle.frame(2, 2, 8, &bytes).unwrap());
        handle.shared.store(handle.frame(1, 2, 8, &bytes).unwrap());
        handle.shared.drain();
        // The newest replaced the one not drawn yet.
        assert_eq!(*heard.borrow(), ["1x2"]);

        attached.set_paused(true);
        handle.shared.store(handle.frame(2, 2, 8, &bytes).unwrap());
        handle.shared.drain();
        assert_eq!(*heard.borrow(), ["1x2"]);
        attached.set_paused(false);
        handle.shared.store(handle.frame(2, 2, 8, &bytes).unwrap());
        handle.shared.drain();
        assert_eq!(*heard.borrow(), ["1x2", "2x2"]);

        // The end comes before a frame of a new registration.
        handle.shared.mark_ended();
        handle.shared.store(handle.frame(2, 2, 8, &bytes).unwrap());
        handle.shared.drain();
        assert_eq!(*heard.borrow(), ["1x2", "2x2", "end", "2x2"]);

        drop(attached);
        handle.shared.store(handle.frame(2, 2, 8, &bytes).unwrap());
        handle.shared.drain();
        assert_eq!(heard.borrow().len(), 4);

        // A paused sink with no frame yet shows the first one that comes,
        // as a paused `<video>` shows its first frame; the next is dropped.
        let (sink, heard) = recorder();
        let attached = streams.attach("cam", sink);
        attached.set_paused(true);
        handle.shared.store(handle.frame(3, 1, 12, &bytes).unwrap());
        handle.shared.drain();
        handle.shared.store(handle.frame(2, 2, 8, &bytes).unwrap());
        handle.shared.drain();
        assert_eq!(*heard.borrow(), ["3x1"]);
        // After the end it is a sink with no frame again.
        handle.shared.mark_ended();
        handle.shared.store(handle.frame(2, 2, 8, &bytes).unwrap());
        handle.shared.drain();
        assert_eq!(*heard.borrow(), ["3x1", "end", "2x2"]);
        assert_eq!(format!("{streams:?}"), r#"{"cam"}"#);
    }

    #[test]
    fn refuses_frames_of_no_size() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        let bytes = vec![0u8; 16];
        assert!(handle.frame(0, 2, 8, &bytes).is_none());
        assert!(handle.frame(2, 0, 8, &bytes).is_none());
        // A stride shorter than the row.
        assert!(handle.frame(3, 1, 8, &bytes).is_none());
        // Fewer bytes than the rows.
        assert!(handle.frame(2, 3, 8, &bytes).is_none());
        assert!(handle.frame(usize::MAX, 2, usize::MAX, &bytes).is_none());
        let frame = handle.frame(2, 2, 8, &bytes).unwrap();
        assert_eq!(frame.bytes.len(), 16);
        // A frame that cannot be shows nothing: nothing is pending.
        handle.push_bgra(0, 0, 0, &bytes);
        assert!(handle.shared.pending.lock().unwrap().frame.is_none());
    }

    #[test]
    fn ending_drops_the_registration() {
        let streams = Streams::default();
        let handle = streams.register("cam");
        assert!(handle.shared.registered.load(Ordering::Acquire));
        let shared = handle.shared.clone();
        drop(handle);
        assert!(!shared.registered.load(Ordering::Acquire));
        // Registered again: the same entry, live again.
        let again = streams.register("cam");
        assert!(Arc::ptr_eq(&again.shared, &shared));
        assert!(shared.registered.load(Ordering::Acquire));
    }
}
