//! Live streams the embedding app registers by name, which `<Video
//! srcObject>` plays (`MediaStream.fromHost(name)`, `VideoProps::stream`):
//! the host pushes frames (`CVPixelBuffer`s) from any thread, and each
//! video showing the stream draws them in an `AVSampleBufferDisplayLayer`
//! of its own, on the main thread. The frames never pass through JS.
//!
//! A name is one entry ([`Shared`]) whether the host registered it or a
//! video asked for it first: a video attaches its layer to the entry, and
//! the frames of a registration that comes later reach it. A name nothing
//! registered is logged once, as an unknown native component is.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::{
    Encode, Encoding, MainThreadMarker, encode::RefEncode, msg_send, rc::Retained,
    runtime::AnyObject,
};

/// CoreVideo's `CVPixelBufferRef`: a frame the embedding app hands a
/// stream, retained while NSH holds it.
pub type CVPixelBufferRef = *mut c_void;

/// CoreMedia's `CMTime`; a zeroed one is `kCMTimeInvalid`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

// SAFETY: the layout and encoding of CoreMedia's `CMTime` (`{?=qiIq}`).
unsafe impl Encode for CMTime {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
    );
}

/// CoreMedia's `CMSampleTimingInfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation: CMTime,
    decode: CMTime,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionarySetValue(dict: *const c_void, key: *const c_void, value: *const c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetWidth(buffer: *const c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *const c_void) -> usize;
    fn CVPixelBufferCreate(
        allocator: *const c_void,
        width: usize,
        height: usize,
        format: u32,
        attributes: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut u8;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
}

/// `kCVPixelFormatType_32BGRA`.
const BGRA: u32 = 0x4247_5241;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_DisplayImmediately: *const c_void;
    fn CMVideoFormatDescriptionCreateForImageBuffer(
        allocator: *const c_void,
        image_buffer: *const c_void,
        out: *mut *const c_void,
    ) -> i32;
    fn CMSampleBufferCreateReadyWithImageBuffer(
        allocator: *const c_void,
        image_buffer: *const c_void,
        format: *const c_void,
        timing: *const CMSampleTimingInfo,
        out: *mut *const c_void,
    ) -> i32;
    fn CMSampleBufferGetSampleAttachmentsArray(
        buffer: *const c_void,
        create: bool,
    ) -> *const c_void;
}

/// A frame: a retained `CVPixelBuffer`.
struct Frame(NonNull<c_void>);

// SAFETY: a CVPixelBuffer is a CoreFoundation object, which any thread may
// hold and release.
unsafe impl Send for Frame {}

impl Frame {
    /// Retains `buffer`; `None` for a null pointer.
    ///
    /// # Safety
    ///
    /// `buffer` is a `CVPixelBufferRef` or null.
    unsafe fn retain(buffer: CVPixelBufferRef) -> Option<Self> {
        let buffer = NonNull::new(buffer)?;
        // SAFETY: a live CF object, as the caller guarantees.
        unsafe { CFRetain(buffer.as_ptr()) };
        Some(Frame(buffer))
    }

    /// A new BGRA buffer of `width` × `height` pixels holding `bytes`
    /// (`stride` bytes a row), or `None` when CoreVideo refuses the size.
    fn from_bgra(width: usize, height: usize, stride: usize, bytes: &[u8]) -> Option<Self> {
        let mut buffer: *mut c_void = std::ptr::null_mut();
        // SAFETY: CoreVideo's documented API; the buffer is locked while
        // its rows are written, and comes retained (+1), which `Frame`
        // releases.
        unsafe {
            if CVPixelBufferCreate(
                std::ptr::null(),
                width,
                height,
                BGRA,
                std::ptr::null(),
                &mut buffer,
            ) != 0
            {
                return None;
            }
            let buffer = NonNull::new(buffer)?;
            CVPixelBufferLockBaseAddress(buffer.as_ptr(), 0);
            let base = CVPixelBufferGetBaseAddress(buffer.as_ptr());
            let own = CVPixelBufferGetBytesPerRow(buffer.as_ptr());
            for y in 0..height {
                std::ptr::copy_nonoverlapping(
                    bytes[y * stride..].as_ptr(),
                    base.add(y * own),
                    width * 4,
                );
            }
            CVPixelBufferUnlockBaseAddress(buffer.as_ptr(), 0);
            Some(Frame(buffer))
        }
    }

    fn size(&self) -> (usize, usize) {
        // SAFETY: a retained CVPixelBuffer.
        unsafe {
            (
                CVPixelBufferGetWidth(self.0.as_ptr()),
                CVPixelBufferGetHeight(self.0.as_ptr()),
            )
        }
    }

    /// A sample buffer of the frame to display at once, or `None` when
    /// CoreMedia refuses the buffer (not a video pixel format).
    fn sample(&self) -> Option<Sample> {
        let mut format: *const c_void = std::ptr::null();
        let mut sample: *const c_void = std::ptr::null();
        // SAFETY: CoreMedia's documented API on a retained CVPixelBuffer;
        // what it creates is released by `Sample`'s drop.
        unsafe {
            if CMVideoFormatDescriptionCreateForImageBuffer(
                std::ptr::null(),
                self.0.as_ptr(),
                &mut format,
            ) != 0
                || format.is_null()
            {
                return None;
            }
            let timing = CMSampleTimingInfo::default();
            let made = CMSampleBufferCreateReadyWithImageBuffer(
                std::ptr::null(),
                self.0.as_ptr(),
                format,
                &timing,
                &mut sample,
            );
            CFRelease(format);
            if made != 0 || sample.is_null() {
                return None;
            }
            // Shown as it comes: a live stream has no timeline.
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, true);
            if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
                CFDictionarySetValue(
                    CFArrayGetValueAtIndex(attachments, 0),
                    kCMSampleAttachmentKey_DisplayImmediately,
                    kCFBooleanTrue,
                );
            }
        }
        Some(Sample(sample))
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: retained in `retain`.
        unsafe { CFRelease(self.0.as_ptr()) };
    }
}

/// CoreMedia's `opaqueCMSampleBuffer`: what a `CMSampleBufferRef` points
/// to, with the encoding `enqueueSampleBuffer:` declares (a pointer to it,
/// `^{opaqueCMSampleBuffer=}`), which objc2 checks in debug builds.
#[repr(C)]
struct OpaqueCMSampleBuffer {
    _private: [u8; 0],
}

// SAFETY: the pointer's Objective-C encoding, as CoreMedia declares the
// struct (opaque, no fields).
unsafe impl RefEncode for OpaqueCMSampleBuffer {
    const ENCODING_REF: Encoding =
        Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
}

/// A `CMSampleBuffer` made for one frame.
struct Sample(*const c_void);

impl Sample {
    /// The buffer as `enqueueSampleBuffer:` takes it.
    fn as_ref(&self) -> *const OpaqueCMSampleBuffer {
        self.0.cast()
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        // SAFETY: created in `Frame::sample`.
        unsafe { CFRelease(self.0) };
    }
}

/// A video showing a stream: its display layer and what it hears.
struct Sink {
    key: u64,
    /// The `AVSampleBufferDisplayLayer`.
    layer: Retained<AnyObject>,
    /// `paused`: frames are dropped, the last one stays.
    paused: Cell<bool>,
    /// Whether the first frame's size was reported.
    loaded: Cell<bool>,
    /// Hears the first frame's size (pixels): the video's `onLoad`.
    on_size: Box<dyn Fn(usize, usize)>,
}

/// What a stream's main-thread side keeps: the videos showing it.
#[derive(Default)]
struct Sinks {
    sinks: Vec<Sink>,
}

/// One stream by name, shared by its registration and the videos showing
/// it. Frames come from any thread; the layers are drawn on the main
/// thread, one dispatch per batch of frames (a frame pushed before the
/// previous one was drawn replaces it: a live stream shows the newest).
pub(crate) struct Shared {
    name: Arc<str>,
    /// The frame waiting to be drawn.
    pending: Mutex<Option<Frame>>,
    /// A draw is queued on the main thread for `pending`.
    scheduled: AtomicBool,
    /// Whether a registration is live (frames may come).
    registered: AtomicBool,
    /// A frame of bytes that did not fit its size was reported.
    wrong_frame: AtomicBool,
    /// How many videos show the stream (the sinks' count, readable off
    /// the main thread): with none, a frame waits without a dispatch.
    attached: AtomicUsize,
    main: MainThreadBound<RefCell<Sinks>>,
}

impl Shared {
    fn new(name: Arc<str>, mtm: MainThreadMarker) -> Arc<Self> {
        Arc::new(Shared {
            name,
            pending: Mutex::new(None),
            scheduled: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            wrong_frame: AtomicBool::new(false),
            attached: AtomicUsize::new(0),
            main: MainThreadBound::new(RefCell::default(), mtm),
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
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        self.schedule();
    }

    /// One draw of the pending frame on the main thread, unless one is on
    /// its way.
    fn schedule(self: &Arc<Self>) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = self.clone();
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("main queue");
            shared.scheduled.store(false, Ordering::Release);
            let frame = shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(frame) = frame {
                shared.draw(mtm, &frame);
            }
        });
    }

    /// Draws `frame` in every sink that is not paused; a paused sink that
    /// has no frame yet draws this one, so a paused video shows the first
    /// frame and reports its size, as a paused `<video>` does.
    fn draw(&self, mtm: MainThreadMarker, frame: &Frame) {
        let sinks = self.main.get(mtm).borrow();
        let mut sample = None;
        for sink in sinks
            .sinks
            .iter()
            .filter(|s| !s.paused.get() || !s.loaded.get())
        {
            if sample.is_none() {
                sample = frame.sample();
            }
            let Some(sample) = &sample else { return };
            // SAFETY: AVSampleBufferDisplayLayer's documented API; the
            // sample buffer is live until `sample` drops.
            unsafe {
                // 2: failed (a buffer it could not decode): flush and go on.
                let status: isize = msg_send![&sink.layer, status];
                if status == 2 {
                    let _: () = msg_send![&sink.layer, flush];
                }
                let _: () = msg_send![&sink.layer, enqueueSampleBuffer: sample.as_ref()];
            }
            if !sink.loaded.replace(true) {
                let (width, height) = frame.size();
                (sink.on_size)(width, height);
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
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("main queue");
            for sink in &shared.main.get(mtm).borrow().sinks {
                // SAFETY: AVSampleBufferDisplayLayer's documented API.
                let _: () = unsafe { msg_send![&sink.layer, flushAndRemoveImage] };
                sink.loaded.set(false);
            }
        });
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

    /// Shows `frame` in the videos playing the stream. Retains it; the
    /// caller keeps its own reference. A frame that comes before the
    /// previous one was drawn replaces it.
    ///
    /// # Safety
    ///
    /// `frame` is a `CVPixelBufferRef` (null shows nothing).
    pub unsafe fn push(&self, frame: CVPixelBufferRef) {
        // SAFETY: guaranteed by the caller.
        if let Some(frame) = unsafe { Frame::retain(frame) } {
            self.shared.push(frame);
        }
    }

    /// Shows a frame given as bytes: `width` × `height` pixels of BGRA (8
    /// bits each), `stride` bytes a row (at least `width * 4`), copied into
    /// a `CVPixelBuffer`, as the other platforms' handles take frames. A
    /// frame that comes before the previous one was drawn replaces it. A
    /// size the bytes do not cover, or an empty one, is logged once and
    /// shows nothing.
    pub fn push_bgra(&self, width: usize, height: usize, stride: usize, bytes: &[u8]) {
        let fits = width > 0
            && height > 0
            && width.checked_mul(4).is_some_and(|row| stride >= row)
            && stride
                .checked_mul(height)
                .is_some_and(|all| bytes.len() >= all);
        if !fits {
            if !self.shared.wrong_frame.swap(true, Ordering::AcqRel) {
                log::warn!(
                    "stream `{}`: a frame of {width}×{height} pixels with {stride} bytes a row \
                     does not fit {} bytes: shown nothing",
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
        if let Some(frame) = Frame::from_bgra(width, height, stride, bytes) {
            self.shared.push(frame);
        }
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

/// A video's attachment to a stream, by name ([`Streams::attach`]); dropping
/// it detaches the video's layer.
pub struct Attached {
    shared: Arc<Shared>,
    key: u64,
}

impl Attached {
    /// The stream's name.
    pub fn name(&self) -> &Arc<str> {
        &self.shared.name
    }

    /// Stops or resumes drawing frames in the video's layer (the last one
    /// stays).
    pub fn set_paused(&self, paused: bool) {
        let mtm = MainThreadMarker::new().expect("main thread");
        for sink in &self.shared.main.get(mtm).borrow().sinks {
            if sink.key == self.key {
                sink.paused.set(paused);
            }
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let mtm = MainThreadMarker::new().expect("main thread");
        let mut sinks = self.shared.main.get(mtm).borrow_mut();
        sinks.sinks.retain(|s| s.key != self.key);
        self.shared
            .attached
            .store(sinks.sinks.len(), Ordering::Release);
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// The streams of one surface, by name (shared with its videos, so a
/// registration applies to videos showing the name from then on, and to
/// those already showing it). Main thread.
#[derive(Clone, Default)]
pub struct Streams {
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
    fn entry(&self, name: &str, mtm: MainThreadMarker) -> Arc<Shared> {
        let mut entries = self.entries.borrow_mut();
        if let Some(shared) = entries.get(name) {
            return shared.clone();
        }
        let name: Arc<str> = name.into();
        let shared = Shared::new(name.clone(), mtm);
        entries.insert(name, shared.clone());
        shared
    }

    /// Registers stream `name` (replacing an earlier registration): the
    /// handle takes its frames. The videos whose `srcObject` names it,
    /// now or later, play it. Main thread.
    pub fn register(&self, name: &str) -> StreamHandle {
        let mtm = MainThreadMarker::new().expect("register a stream on the main thread");
        let shared = self.entry(name, mtm);
        shared.registered.store(true, Ordering::Release);
        self.missing.borrow_mut().remove(name);
        StreamHandle { shared }
    }

    /// Attaches `layer`, a video's `AVSampleBufferDisplayLayer`, to stream
    /// `name`: it draws the frames, and `on_size` hears the first frame's
    /// size (pixels). A name nothing registered is logged once; the layer
    /// draws once something is.
    pub fn attach(
        &self,
        name: &str,
        layer: Retained<AnyObject>,
        on_size: Box<dyn Fn(usize, usize)>,
    ) -> Attached {
        let mtm = MainThreadMarker::new().expect("main thread");
        let shared = self.entry(name, mtm);
        if !shared.registered.load(Ordering::Acquire)
            && self.missing.borrow_mut().insert(shared.name.clone())
        {
            log::warn!(
                target: "js",
                "no stream is registered as `{name}`: the video shows nothing"
            );
        }
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        {
            let mut sinks = shared.main.get(mtm).borrow_mut();
            sinks.sinks.push(Sink {
                key,
                layer,
                paused: Cell::new(false),
                loaded: Cell::new(false),
                on_size,
            });
            shared.attached.store(sinks.sinks.len(), Ordering::Release);
        }
        // A frame pushed but not drawn yet shows in the new video at once.
        if shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            shared.schedule();
        }
        Attached { shared, key }
    }
}

/// A new `AVSampleBufferDisplayLayer` (AVFoundation loaded), or `None`
/// where the class is missing.
pub fn display_layer() -> Option<Retained<AnyObject>> {
    static LOADED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let loaded = *LOADED.get_or_init(|| {
        // SAFETY: loads a system framework; the handle is never closed.
        !unsafe {
            libc::dlopen(
                c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation".as_ptr(),
                libc::RTLD_LAZY,
            )
        }
        .is_null()
    });
    if !loaded {
        return None;
    }
    let cls = objc2::runtime::AnyClass::get(c"AVSampleBufferDisplayLayer")?;
    // SAFETY: a CALayer subclass's documented constructor.
    Some(unsafe { msg_send![cls, new] })
}

/// `AVLayerVideoGravity*` for a video's `resize_mode`.
pub fn gravity(mode: nsh_core::ResizeMode) -> Retained<objc2_foundation::NSString> {
    objc2_foundation::NSString::from_str(match mode {
        nsh_core::ResizeMode::Cover => "AVLayerVideoGravityResizeAspectFill",
        nsh_core::ResizeMode::Stretch => "AVLayerVideoGravityResize",
        _ => "AVLayerVideoGravityResizeAspect",
    })
}
