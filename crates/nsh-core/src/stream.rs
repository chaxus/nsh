//! Live streams without a toolkit: the registry the native backends share
//! for `<Video srcObject={MediaStream.fromHost(name)}>` (`VideoProps::stream`).
//! The embedding app registers a stream by name and pushes frames from any
//! thread ([`StreamHandle`]); each video naming the stream attaches a sink
//! ([`Streams::attach`]) that draws them on the main thread. The frames
//! never pass through JS.
//!
//! What every backend does the same way lives here: a name is one entry
//! whether the host registered it or a video asked for it first (a video
//! attached before the registration shows the stream once it comes); a name
//! nothing registered is logged once, as an unknown native component is;
//! the newest frame wins (a frame pushed before the previous one was drawn
//! replaces it, so a slow main thread drops frames rather than queueing
//! them), delivered with one hop to the main thread per batch; `paused`
//! sinks keep their last frame, but one with no frame yet still takes the
//! first that comes, as a paused `<video>` shows its first frame; the end of
//! a registration blanks the videos until the name is registered again; a
//! frame whose bytes do not fit its size is logged once and shows nothing;
//! and while no video shows the stream a push costs nothing: the bytes are
//! not copied and the main thread is not woken.
//!
//! A backend gives the frame type `F` it draws (a `CVPixelBuffer`, a
//! `Vec<u8>`, a `glib::Bytes`, a GPUI `RenderImage`), made once per push on
//! the pushing thread, and the [`Hop`] to its main thread. The sinks are
//! the backend's closures and need not be `Send`: they are only ever touched
//! on the thread that made the registry (attach, detach, pause, deliver),
//! which [`MainThreadCell`] holds them to.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::ThreadId,
};

/// What a stream tells the videos showing it, on the main thread.
pub enum StreamEvent<'a, F> {
    /// A new frame to show.
    Frame {
        /// The frame.
        frame: &'a F,
        /// The sink's first frame since it attached or the stream last
        /// ended: the video's `onLoad`, with the frame's size.
        first: bool,
    },
    /// The registration ended: show nothing until another frame comes.
    Ended,
}

/// Hears a stream's events: a video's closure, called on the main thread.
pub type Sink<F> = Box<dyn Fn(StreamEvent<'_, F>)>;

/// One hop to the backend's main thread: runs `job` there, soon and in
/// order with other hops. `false` when it could not be queued (the thread
/// is gone, or not started): the job is dropped and the registry tries
/// again with the next push.
pub trait Hop: Send + Sync + 'static {
    /// Runs `job` on the main thread.
    fn invoke(&self, job: Box<dyn FnOnce() + Send>) -> bool;
}

impl<T: Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync + 'static> Hop for T {
    fn invoke(&self, job: Box<dyn FnOnce() + Send>) -> bool {
        self(job)
    }
}

/// A value only the thread that made it touches (the main thread's sinks),
/// kept in a structure other threads share. Debug builds check the thread.
struct MainThreadCell<T> {
    thread: ThreadId,
    value: RefCell<T>,
}

// SAFETY: every access goes through `get`, which asserts the thread in
// debug builds; the registry's contract is that sinks are attached,
// detached, paused and told on the one thread that made the registry.
unsafe impl<T> Send for MainThreadCell<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for MainThreadCell<T> {}

impl<T> MainThreadCell<T> {
    fn new(value: T) -> Self {
        MainThreadCell {
            thread: std::thread::current().id(),
            value: RefCell::new(value),
        }
    }

    fn get(&self) -> &RefCell<T> {
        debug_assert_eq!(
            std::thread::current().id(),
            self.thread,
            "a stream's sinks are the main thread's"
        );
        &self.value
    }
}

/// A video showing a stream: its sink and what it hears. The flags are
/// cells, so a sink may pause its own attachment from inside.
struct Attachment<F> {
    key: u64,
    /// `paused`: frames are dropped, the last one stays.
    paused: Cell<bool>,
    /// A frame was delivered since the registration started or last ended.
    loaded: Cell<bool>,
    sink: Sink<F>,
}

/// One stream by name, shared by its registration and the videos showing
/// it.
struct Shared<F> {
    name: Arc<str>,
    hop: Arc<dyn Hop>,
    /// The frame waiting to be delivered.
    pending: Mutex<Option<F>>,
    /// A delivery is queued on the main thread for `pending`.
    scheduled: AtomicBool,
    /// Whether a registration is live (frames may come).
    registered: AtomicBool,
    /// A wrong push was reported (once per stream).
    warned: AtomicBool,
    /// The registration ended but the hop refused the job: the sinks hear
    /// the end with the next delivery that reaches the main thread.
    ended: AtomicBool,
    /// How many videos show the stream (readable off the main thread).
    attached: AtomicUsize,
    /// The videos showing the stream, main thread only.
    sinks: MainThreadCell<Vec<Rc<Attachment<F>>>>,
}

impl<F: Send + 'static> Shared<F> {
    fn new(name: Arc<str>, hop: Arc<dyn Hop>) -> Arc<Self> {
        Arc::new(Shared {
            name,
            hop,
            pending: Mutex::new(None),
            scheduled: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            attached: AtomicUsize::new(0),
            sinks: MainThreadCell::new(Vec::new()),
        })
    }

    /// Whether frames are wanted: a registration is live and a video shows
    /// the stream.
    fn wanted(&self) -> bool {
        self.registered.load(Ordering::Acquire) && self.attached.load(Ordering::Acquire) > 0
    }

    /// Keeps `frame` for the next delivery, in place of one not delivered
    /// yet, and queues the delivery.
    fn push(self: &Arc<Self>, frame: F) {
        if !self.wanted() {
            return;
        }
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        self.schedule();
    }

    /// One delivery on the main thread, unless one is on its way.
    fn schedule(self: &Arc<Self>) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = self.clone();
        if !self.hop.invoke(Box::new(move || shared.deliver())) {
            self.scheduled.store(false, Ordering::Release);
        }
    }

    /// On the main thread: an end the hop refused earlier first, then the
    /// pending frame to every sink that is not paused (and to a paused one
    /// with no frame yet).
    fn deliver(&self) {
        self.scheduled.store(false, Ordering::Release);
        if self.ended.swap(false, Ordering::AcqRel) {
            self.tell_ended();
        }
        let frame = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(frame) = frame else { return };
        // Cloned out, so a sink that attaches or detaches while told does
        // not meet the borrow.
        let sinks = self.sinks.get().borrow().clone();
        for attachment in sinks {
            if !attachment.paused.get() || !attachment.loaded.get() {
                let first = !attachment.loaded.replace(true);
                (attachment.sink)(StreamEvent::Frame {
                    frame: &frame,
                    first,
                });
            }
        }
    }

    /// On the main thread: the sinks hear the end.
    fn tell_ended(&self) {
        let sinks = self.sinks.get().borrow().clone();
        for attachment in sinks {
            attachment.loaded.set(false);
            (attachment.sink)(StreamEvent::Ended);
        }
    }

    /// The registration ended: the videos show nothing until another. When
    /// the hop cannot take the job (the main thread is not up), the end
    /// waits for the next delivery that reaches it.
    fn end(self: &Arc<Self>) {
        self.registered.store(false, Ordering::Release);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let shared = self.clone();
        if !self.hop.invoke(Box::new(move || shared.tell_ended())) {
            self.ended.store(true, Ordering::Release);
        }
    }
}

/// A stream the embedding app registered ([`Streams::register`]): where
/// it pushes the frames, from any thread. Dropping it ends the stream.
#[non_exhaustive]
pub struct StreamHandle<F: Send + 'static> {
    shared: Arc<Shared<F>>,
}

impl<F: Send + 'static> std::fmt::Debug for StreamHandle<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle")
            .field("name", &self.shared.name)
            .finish()
    }
}

impl<F: Send + 'static> StreamHandle<F> {
    /// The name it was registered as.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Whether a frame pushed now would show: a video shows the stream and
    /// the registration is live. A host that makes its frames at a cost
    /// may skip them while this is `false`.
    pub fn wanted(&self) -> bool {
        self.shared.wanted()
    }

    /// Shows `frame` in the videos playing the stream (the newest wins).
    pub fn push(&self, frame: F) {
        self.shared.push(frame);
    }

    /// Shows a frame given as bytes: `width` × `height` pixels of BGRA (8
    /// bits each, opaque or premultiplied), `stride` bytes per row (at
    /// least `width * 4`), which `make` turns into the backend's frame
    /// (copying them) once the size is checked and a video wants it. A
    /// frame that does not fit its size (or has none) is logged once per
    /// stream and shows nothing; so does one `make` refuses.
    pub fn push_bgra(
        &self,
        width: usize,
        height: usize,
        stride: usize,
        bytes: &[u8],
        make: impl FnOnce(usize, usize, usize, &[u8]) -> Option<F>,
    ) {
        if let Err(what) = check_bgra(width, height, stride, bytes.len()) {
            if !self.shared.warned.swap(true, Ordering::AcqRel) {
                log::warn!(
                    "stream `{}`: a frame of {width}×{height} pixels with {stride} bytes per row \
                     and {} bytes in all shows nothing: {what}",
                    self.shared.name,
                    bytes.len()
                );
            }
            return;
        }
        // No video shows the stream: the bytes are not even copied.
        if !self.shared.wanted() {
            return;
        }
        let Some(frame) = make(width, height, stride, bytes) else {
            if !self.shared.warned.swap(true, Ordering::AcqRel) {
                log::warn!(
                    "stream `{}`: a frame of {width}×{height} pixels cannot be made: it shows nothing",
                    self.shared.name
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

impl<F: Send + 'static> Drop for StreamHandle<F> {
    fn drop(&mut self) {
        self.shared.end();
    }
}

/// Why a frame of `width` × `height` pixels, `stride` bytes per row and
/// `len` bytes in all cannot be shown, if it cannot.
pub fn check_bgra(
    width: usize,
    height: usize,
    stride: usize,
    len: usize,
) -> Result<(), &'static str> {
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

/// A video's attachment to a stream ([`Streams::attach`]); dropping it
/// detaches the video's sink. Main thread.
#[non_exhaustive]
pub struct Attached<F> {
    shared: Arc<Shared<F>>,
    attachment: Rc<Attachment<F>>,
}

impl<F> Attached<F> {
    /// The stream's name.
    pub fn name(&self) -> &Arc<str> {
        &self.shared.name
    }

    /// Stops or resumes delivering frames to the video's sink (the last
    /// one stays; a video without one yet still takes the first).
    pub fn set_paused(&self, paused: bool) {
        self.attachment.paused.set(paused);
    }
}

impl<F> Drop for Attached<F> {
    fn drop(&mut self) {
        let key = self.attachment.key;
        let sinks = self.shared.sinks.get();
        let mut sinks = sinks.borrow_mut();
        sinks.retain(|a| a.key != key);
        self.shared.attached.store(sinks.len(), Ordering::Release);
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// A registry's entries by name, shared with its clones.
type Entries<F> = Rc<RefCell<HashMap<Arc<str>, Arc<Shared<F>>>>>;

/// The streams of one registry (a backend's `Components`), by name: shared
/// with its videos, so a registration applies to videos showing the name
/// from then on, and to those already showing it. Main thread.
#[non_exhaustive]
pub struct Streams<F> {
    entries: Entries<F>,
    /// Names already reported as unregistered.
    missing: Rc<RefCell<HashSet<Arc<str>>>>,
    hop: Arc<dyn Hop>,
}

impl<F> Clone for Streams<F> {
    fn clone(&self) -> Self {
        Streams {
            entries: self.entries.clone(),
            missing: self.missing.clone(),
            hop: self.hop.clone(),
        }
    }
}

impl<F> std::fmt::Debug for Streams<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.entries.borrow().keys()).finish()
    }
}

impl<F: Send + 'static> Streams<F> {
    /// A registry whose deliveries reach the main thread through `hop`.
    pub fn new(hop: impl Hop) -> Self {
        Streams {
            entries: Rc::default(),
            missing: Rc::default(),
            hop: Arc::new(hop),
        }
    }

    /// The entry of `name`, made if none.
    fn entry(&self, name: &str) -> Arc<Shared<F>> {
        let mut entries = self.entries.borrow_mut();
        if let Some(shared) = entries.get(name) {
            return shared.clone();
        }
        let name: Arc<str> = name.into();
        let shared = Shared::new(name.clone(), self.hop.clone());
        entries.insert(name, shared.clone());
        shared
    }

    /// Registers stream `name` (replacing an earlier registration): the
    /// handle takes its frames. The videos whose `srcObject` names it, now
    /// or later, play it.
    pub fn register(&self, name: &str) -> StreamHandle<F> {
        let shared = self.entry(name);
        shared.registered.store(true, Ordering::Release);
        self.missing.borrow_mut().remove(name);
        StreamHandle { shared }
    }

    /// Attaches `sink`, a video's, to stream `name`: it hears the frames
    /// and the end on this thread. A name nothing registered is logged
    /// once; the sink hears frames once something is. A frame pushed but
    /// not delivered yet shows in the video at once.
    pub fn attach(&self, name: &str, sink: Sink<F>) -> Attached<F> {
        let shared = self.entry(name);
        if !shared.registered.load(Ordering::Acquire)
            && self.missing.borrow_mut().insert(shared.name.clone())
        {
            log::warn!(
                target: "js",
                "no stream is registered as `{name}`: the video shows nothing"
            );
        }
        let attachment = Rc::new(Attachment {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            paused: Cell::new(false),
            loaded: Cell::new(false),
            sink,
        });
        {
            let sinks = shared.sinks.get();
            let mut sinks = sinks.borrow_mut();
            sinks.push(attachment.clone());
            shared.attached.store(sinks.len(), Ordering::Release);
        }
        if shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            shared.schedule();
        }
        Attached { shared, attachment }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// A registry whose hop queues the jobs for the test to run (`drain`),
    /// as a main loop would.
    type Jobs = mpsc::Receiver<Box<dyn FnOnce() + Send>>;

    fn streams() -> (Streams<Vec<u8>>, Jobs) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let hop = move |job: Box<dyn FnOnce() + Send>| tx.lock().unwrap().send(job).is_ok();
        (Streams::new(hop), rx)
    }

    fn drain(rx: &Jobs) -> usize {
        let mut n = 0;
        while let Ok(job) = rx.try_recv() {
            job();
            n += 1;
        }
        n
    }

    /// A sink recording the first byte of each frame, 0 for the end.
    fn recorder() -> (Sink<Vec<u8>>, Rc<RefCell<Vec<u8>>>) {
        let heard = Rc::new(RefCell::new(Vec::new()));
        let log = heard.clone();
        let sink: Sink<Vec<u8>> = Box::new(move |event| {
            log.borrow_mut().push(match event {
                StreamEvent::Frame { frame, first } => frame[0] + if first { 100 } else { 0 },
                StreamEvent::Ended => 0,
            });
        });
        (sink, heard)
    }

    fn copy(_: usize, _: usize, _: usize, bytes: &[u8]) -> Option<Vec<u8>> {
        Some(bytes.to_vec())
    }

    #[test]
    fn a_name_is_one_entry_and_the_newest_frame_wins() {
        let (streams, rx) = streams();
        let (sink, heard) = recorder();
        // Attached first: the registration that follows reaches it.
        let attached = streams.attach("cam", sink);
        let handle = streams.register("cam");
        assert_eq!(handle.name(), "cam");
        assert_eq!(&**attached.name(), "cam");
        assert_eq!(streams.entries.borrow().len(), 1);
        assert!(handle.wanted());
        handle.push_bgra(1, 1, 4, &[1, 0, 0, 255], copy);
        handle.push_bgra(1, 1, 4, &[2, 0, 0, 255], copy);
        // One hop for the two frames; only the newest is seen.
        assert_eq!(drain(&rx), 1);
        assert_eq!(*heard.borrow(), [102]);
        drop(attached);
        assert!(!handle.wanted());
        handle.push_bgra(1, 1, 4, &[3, 0, 0, 255], copy);
        assert_eq!(drain(&rx), 0);
        assert_eq!(*heard.borrow(), [102]);
        assert_eq!(format!("{streams:?}"), r#"{"cam"}"#);
    }

    #[test]
    fn paused_sinks_keep_the_last_frame_and_the_end_reaches_all() {
        let (streams, rx) = streams();
        let handle = streams.register("cam");
        let (sink, heard) = recorder();
        let attached = streams.attach("cam", sink);
        // Paused before any frame: the first one still shows, the next is
        // dropped.
        attached.set_paused(true);
        handle.push_bgra(1, 1, 4, &[7, 0, 0, 255], copy);
        drain(&rx);
        handle.push_bgra(1, 1, 4, &[8, 0, 0, 255], copy);
        drain(&rx);
        assert_eq!(*heard.borrow(), [107]);
        attached.set_paused(false);
        handle.push_bgra(1, 1, 4, &[9, 0, 0, 255], copy);
        drain(&rx);
        assert_eq!(*heard.borrow(), [107, 9]);
        // Ended: a pending frame goes, pushes after it show nothing, the
        // sink is a sink with no frame again.
        attached.set_paused(true);
        handle.push_bgra(1, 1, 4, &[1, 0, 0, 255], copy);
        handle.end();
        drain(&rx);
        assert_eq!(*heard.borrow(), [107, 9, 0]);
        handle.push_bgra(1, 1, 4, &[1, 0, 0, 255], copy);
        assert_eq!(drain(&rx), 0);
        // Registered again: the same entry, frames flow, the first shows
        // in the paused sink.
        let again = streams.register("cam");
        again.push_bgra(1, 1, 4, &[2, 0, 0, 255], copy);
        drain(&rx);
        assert_eq!(*heard.borrow(), [107, 9, 0, 102]);
        assert_eq!(streams.entries.borrow().len(), 1);
    }

    #[test]
    fn a_wrong_push_shows_nothing_and_does_not_panic() {
        let (streams, rx) = streams();
        let handle = streams.register("cam");
        let (sink, heard) = recorder();
        let _attached = streams.attach("cam", sink);
        // Too few bytes, a stride narrower than a row, no size, overflow.
        handle.push_bgra(2, 2, 8, &[0; 12], copy);
        handle.push_bgra(2, 2, 4, &[0; 32], copy);
        handle.push_bgra(0, 2, 8, &[0; 32], copy);
        handle.push_bgra(usize::MAX, 1, usize::MAX, &[0; 32], copy);
        // A frame the backend cannot make.
        handle.push_bgra(2, 2, 8, &[0; 32], |_, _, _, _| None);
        assert_eq!(drain(&rx), 0);
        assert!(handle.shared.warned.load(Ordering::Acquire));
        // A right one after them still shows.
        handle.push_bgra(2, 2, 8, &[5; 32], copy);
        drain(&rx);
        assert_eq!(*heard.borrow(), [105]);
    }

    #[test]
    fn a_pending_frame_reaches_a_sink_attaching_before_the_hop() {
        let (streams, rx) = streams();
        let handle = streams.register("cam");
        let (first, heard_first) = recorder();
        let _a = streams.attach("cam", first);
        handle.push_bgra(1, 1, 4, &[4, 0, 0, 255], copy);
        // Attached while the delivery waits: it hears the frame too.
        let (second, heard_second) = recorder();
        let _b = streams.attach("cam", second);
        drain(&rx);
        assert_eq!(*heard_first.borrow(), [104]);
        assert_eq!(*heard_second.borrow(), [104]);
    }

    #[test]
    fn dropping_the_handle_ends_the_stream() {
        let (streams, rx) = streams();
        let (sink, heard) = recorder();
        let _attached = streams.attach("cam", sink);
        let handle = streams.register("cam");
        let shared = handle.shared.clone();
        drop(handle);
        drain(&rx);
        assert!(!shared.registered.load(Ordering::Acquire));
        assert_eq!(*heard.borrow(), [0]);
        let again = streams.register("cam");
        assert!(Arc::ptr_eq(&again.shared, &shared));
        assert!(again.wanted());
    }

    #[test]
    fn a_hop_that_fails_lets_the_next_push_try_again() {
        let failing = Streams::<Vec<u8>>::new(|_: Box<dyn FnOnce() + Send>| false);
        let (sink, _heard) = recorder();
        let _attached = failing.attach("cam", sink);
        let handle = failing.register("cam");
        handle.push_bgra(1, 1, 4, &[1, 0, 0, 255], copy);
        assert!(!handle.shared.scheduled.load(Ordering::Acquire));
    }

    #[test]
    fn an_end_the_hop_refused_reaches_the_sinks_with_the_next_delivery() {
        // A hop that refuses until the "main thread" is up.
        let up = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (tx, up_hop) = (Mutex::new(tx), up.clone());
        let streams = Streams::<Vec<u8>>::new(move |job: Box<dyn FnOnce() + Send>| {
            up_hop.load(Ordering::Acquire) && tx.lock().unwrap().send(job).is_ok()
        });
        let (sink, heard) = recorder();
        let attached = streams.attach("cam", sink);
        attached.set_paused(true);
        let handle = streams.register("cam");
        drop(handle);
        assert!(streams.entry("cam").ended.load(Ordering::Acquire));
        up.store(true, Ordering::Release);
        // Registered again: the first delivery tells the end first, then
        // the frame, a first one again for the paused sink.
        let again = streams.register("cam");
        again.push_bgra(1, 1, 4, &[3, 0, 0, 255], copy);
        drain(&rx);
        assert_eq!(*heard.borrow(), [0, 103]);
    }

    #[test]
    fn checks_frames() {
        assert_eq!(check_bgra(2, 2, 8, 16), Ok(()));
        assert_eq!(check_bgra(2, 2, 12, 30), Ok(()));
        assert!(check_bgra(0, 2, 8, 16).is_err());
        assert!(check_bgra(2, 0, 8, 16).is_err());
        assert!(check_bgra(2, 2, 7, 16).is_err());
        assert!(check_bgra(2, 2, 8, 15).is_err());
        assert!(check_bgra(usize::MAX, 1, usize::MAX, usize::MAX).is_err());
    }
}
