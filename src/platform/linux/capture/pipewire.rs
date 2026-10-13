//! A PipeWire video stream consumer, loaded at run time.
//!
//! `libpipewire-0.3.so.0` is opened with dlopen rather than linked: every
//! desktop with the ScreenCast portal has it, but its development headers (and
//! the Rust bindings generated from them) track newer releases than long-term
//! distributions ship. The few entry points used here have been stable since
//! PipeWire 0.3, and the SPA format descriptions are written by hand.
//!
//! Frames are requested in shared memory (MemPtr/MemFd), read on PipeWire's
//! loop thread into BGRA and handed to the capture thread as the latest
//! picture; DMA-BUF import belongs with a GPU encoder.
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// --- SPA / PipeWire ABI (spa-0.2, pipewire-0.3) --------------------------------

#[repr(C)]
struct SpaList {
    next: *mut c_void,
    prev: *mut c_void,
}
#[repr(C)]
struct SpaCallbacks {
    funcs: *const c_void,
    data: *mut c_void,
}
#[repr(C)]
struct SpaHook {
    link: SpaList,
    cb: SpaCallbacks,
    removed: Option<unsafe extern "C" fn(*mut SpaHook)>,
    private: *mut c_void,
}
type Callback = Option<unsafe extern "C" fn(*mut c_void)>;
#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Callback,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    process: Callback,
    drained: Callback,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Callback,
}
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
}
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut SpaMeta,
    datas: *mut SpaData,
}
#[repr(C)]
struct SpaMeta {
    kind: u32,
    size: u32,
    data: *mut c_void,
}
#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}
#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}
#[repr(C)]
struct SpaMetaRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

const STREAM_EVENTS_VERSION: u32 = 2;
const DIRECTION_INPUT: u32 = 0;
const FLAG_AUTOCONNECT: u32 = 1 << 0;
// `FLAG_MAP_BUFFERS` maps every buffer except DMA-BUF, which is exactly the
// split wanted: shared memory is mapped for the CPU copy, and DMA-BUF is left
// as a file descriptor for the encoder to import.
const FLAG_MAP_BUFFERS: u32 = 1 << 2;
const STATE_ERROR: c_int = -1;
const STATE_UNCONNECTED: c_int = 0;
const STATE_STREAMING: c_int = 3;

/// `spa_pod_prop` flags. `DONT_FIXATE` on the modifier is how a client says
/// "here is a list, you pick": the portal looks for exactly that before it
/// will consider DMA-BUF (its log line is "DMA (No fixate)").
const SPA_POD_PROP_FLAG_DONT_FIXATE: u32 = 1 << 4;

const TYPE_ID: u32 = 3;
const TYPE_INT: u32 = 4;
const TYPE_LONG: u32 = 5;
const TYPE_RECTANGLE: u32 = 10;
const TYPE_FRACTION: u32 = 11;
const TYPE_OBJECT: u32 = 15;
const TYPE_CHOICE: u32 = 19;
const OBJECT_FORMAT: u32 = 0x40003;
const OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const OBJECT_PARAM_META: u32 = 0x40005;
const PARAM_ENUM_FORMAT: u32 = 3;
const PARAM_FORMAT: u32 = 4;
const PARAM_BUFFERS: u32 = 5;
const PARAM_META: u32 = 6;
const FORMAT_MEDIA_TYPE: u32 = 1;
const FORMAT_MEDIA_SUBTYPE: u32 = 2;
const FORMAT_VIDEO_FORMAT: u32 = 0x20001;
/// The DRM format modifier. SPA documents it as "use only with DMA-BUF and
/// omit for other buffer types", and that is the whole signal: the portal
/// negotiates shared memory unless this key is present, however the
/// buffer-type mask reads.
const FORMAT_VIDEO_MODIFIER: u32 = 0x20002;
const FORMAT_VIDEO_SIZE: u32 = 0x20003;
const FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const MEDIA_TYPE_VIDEO: u32 = 2;
const MEDIA_SUBTYPE_RAW: u32 = 1;
const VIDEO_RGBX: u32 = 7;
const VIDEO_BGRX: u32 = 8;
const VIDEO_RGBA: u32 = 11;
const VIDEO_BGRA: u32 = 12;

/// DRM format modifiers, spelled the way DRM does. `I915_FORMAT_MOD_Y_TILED`
/// is what i965 exports and its encoder imports.
const DRM_MOD_Y_TILED: u64 = 0x0100_0000_0000_0002;
const DRM_MOD_X_TILED: u64 = 0x0100_0000_0000_0001;
const DRM_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`, which SPA reads as "let the compositor choose".
const DRM_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const BUFFERS_DATA_TYPE: u32 = 6;
const META_TYPE: u32 = 1;
const META_SIZE: u32 = 2;
const META_VIDEO_CROP: u32 = 2;
const DATA_MEM_PTR: u32 = 1;
const DATA_MEM_FD: u32 = 2;
// `SPA_DATA_DmaBuf` is the fourth variant, so the bit is `1 << 3`. Reading the
// enum value as the shift would ask for `SPA_DATA_MemId` instead, which no
// compositor offers: the buffer-type masks then have no intersection and the
// link fails with `error alloc buffers: Invalid argument`.
const DATA_MEM_DMABUF: u32 = 3;
const CHOICE_RANGE: u32 = 1;
const CHOICE_ENUM: u32 = 3;
const CHOICE_FLAGS: u32 = 4;

struct Api {
    _library: libloading::Library,
    thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    context_destroy: unsafe extern "C" fn(*mut c_void),
    context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    properties_new: unsafe extern "C" fn(*const c_char, ...) -> *mut c_void,
    stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    stream_add_listener:
        unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const StreamEvents, *mut c_void),
    stream_connect:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const c_void, u32) -> c_int,
    stream_update_params: unsafe extern "C" fn(*mut c_void, *mut *const c_void, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    stream_destroy: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn load() -> Result<Api> {
    // SAFETY: loading the system PipeWire client library; the symbols below
    // are declared with their C signatures from pipewire-0.3.
    unsafe {
        let library = libloading::Library::new("libpipewire-0.3.so.0")
            .context("没有找到 PipeWire（libpipewire-0.3.so.0）")?;
        macro_rules! symbol {
            ($name:literal) => {
                *library
                    .get(concat!($name, "\0").as_bytes())
                    .with_context(|| format!("PipeWire 缺少 {}", $name))?
            };
        }
        let init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) = symbol!("pw_init");
        init(std::ptr::null_mut(), std::ptr::null_mut());
        Ok(Api {
            thread_loop_new: symbol!("pw_thread_loop_new"),
            thread_loop_get_loop: symbol!("pw_thread_loop_get_loop"),
            thread_loop_start: symbol!("pw_thread_loop_start"),
            thread_loop_stop: symbol!("pw_thread_loop_stop"),
            thread_loop_destroy: symbol!("pw_thread_loop_destroy"),
            thread_loop_lock: symbol!("pw_thread_loop_lock"),
            thread_loop_unlock: symbol!("pw_thread_loop_unlock"),
            context_new: symbol!("pw_context_new"),
            context_destroy: symbol!("pw_context_destroy"),
            context_connect_fd: symbol!("pw_context_connect_fd"),
            core_disconnect: symbol!("pw_core_disconnect"),
            properties_new: symbol!("pw_properties_new"),
            stream_new: symbol!("pw_stream_new"),
            stream_add_listener: symbol!("pw_stream_add_listener"),
            stream_connect: symbol!("pw_stream_connect"),
            stream_update_params: symbol!("pw_stream_update_params"),
            stream_dequeue_buffer: symbol!("pw_stream_dequeue_buffer"),
            stream_queue_buffer: symbol!("pw_stream_queue_buffer"),
            stream_disconnect: symbol!("pw_stream_disconnect"),
            stream_destroy: symbol!("pw_stream_destroy"),
            _library: library,
        })
    }
}

// --- SPA pod encoding ------------------------------------------------------------

/// Builds one SPA pod in an 8-byte aligned buffer, the layout `spa_pod_builder`
/// produces: every pod is `{u32 size, u32 type}` then its body, padded to 8.
#[derive(Default)]
struct Pod(Vec<u8>);
impl Pod {
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }
    fn pad(&mut self) {
        while self.0.len() % 8 != 0 {
            self.0.push(0);
        }
    }
    fn value(&mut self, kind: u32, body: &[u32]) {
        self.u32((body.len() * 4) as u32);
        self.u32(kind);
        for word in body {
            self.u32(*word);
        }
        self.pad();
    }
    /// A choice of `Long` values, the form SPA uses for DRM modifiers.
    fn choice_long(&mut self, choice: u32, values: &[u64]) {
        self.u32((16 + values.len() * 8) as u32);
        self.u32(TYPE_CHOICE);
        self.u32(choice);
        self.u32(0);
        self.u32(8);
        self.u32(TYPE_LONG);
        for value in values {
            self.u64(*value);
        }
        self.pad();
    }
    /// A choice of `values`, each `child_words` 32-bit words of `child_type`.
    fn choice(&mut self, choice: u32, child_type: u32, child_words: usize, values: &[u32]) {
        self.u32((16 + values.len() * 4) as u32);
        self.u32(TYPE_CHOICE);
        self.u32(choice);
        self.u32(0);
        self.u32((child_words * 4) as u32);
        self.u32(child_type);
        for word in values {
            self.u32(*word);
        }
        self.pad();
    }
    /// An object with properties written by `props`, each started with `key`.
    fn object(kind: u32, id: u32, props: impl FnOnce(&mut Self)) -> Self {
        let mut pod = Self::default();
        pod.u32(0);
        pod.u32(TYPE_OBJECT);
        pod.u32(kind);
        pod.u32(id);
        props(&mut pod);
        let size = (pod.0.len() - 8) as u32;
        pod.0[..4].copy_from_slice(&size.to_ne_bytes());
        pod
    }
    fn key(&mut self, key: u32) {
        self.key_flags(key, 0);
    }
    /// A property key with `spa_pod_prop` flags. `DONT_FIXATE` is how a client
    /// says "here is a list, you choose" -- the portal looks for exactly that on
    /// the modifier before it will consider DMA-BUF.
    fn key_flags(&mut self, key: u32, flags: u32) {
        self.u32(key);
        self.u32(flags);
    }
    /// The bytes, copied into 8-byte aligned storage as SPA requires.
    fn aligned(&self) -> Vec<u64> {
        let mut words = vec![0u64; self.0.len().div_ceil(8)];
        // SAFETY: `words` has at least as many bytes as the pod.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.0.as_ptr(),
                words.as_mut_ptr().cast::<u8>(),
                self.0.len(),
            );
        }
        words
    }
}

fn enum_format() -> Pod {
    Pod::object(OBJECT_FORMAT, PARAM_ENUM_FORMAT, |p| {
        p.key(FORMAT_MEDIA_TYPE);
        p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
        p.key(FORMAT_MEDIA_SUBTYPE);
        p.value(TYPE_ID, &[MEDIA_SUBTYPE_RAW]);
        // The modifier comes first: SPA treats its presence as the request for
        // DMA-BUF, and omits it for the shared-memory formats below. The portal
        // reads the pair together, so a format with no modifier means
        // shared memory.
        //
        // It is opt-in because the frame still has to be imported from the file
        // descriptor, and until that path exists a DMA-BUF is a frame the
        // capture cannot read: the stream negotiates, then never produces one.
        if dmabuf_requested() {
            p.key_flags(FORMAT_VIDEO_MODIFIER, SPA_POD_PROP_FLAG_DONT_FIXATE);
            // Only modifiers whose layout the importer understands. Offering
            // `DRM_FORMAT_MOD_INVALID` invites the portal to fixate to it, and
            // an unfixed modifier says nothing about the tiling, so the import
            // would read the wrong bytes.
            p.choice_long(CHOICE_ENUM, &[DRM_MOD_LINEAR, DRM_MOD_X_TILED, DRM_MOD_Y_TILED]);
        }
        // The format stays a `spa_video_format`: the compositor offers AR24 and
        // AB24 from wlr, and the portal reports those as `BGRx`/`RGBA`-family
        // enums with the modifier carrying the tiling. A DRM fourcc here is a
        // different, unused encoding and never matches.
        p.key(FORMAT_VIDEO_FORMAT);
        p.choice(
            CHOICE_ENUM,
            TYPE_ID,
            1,
            &[VIDEO_BGRA, VIDEO_RGBA, VIDEO_BGRX, VIDEO_RGBX],
        );
        p.key(FORMAT_VIDEO_SIZE);
        p.choice(
            CHOICE_RANGE,
            TYPE_RECTANGLE,
            2,
            &[1920, 1080, 1, 1, 16384, 16384],
        );
        p.key(FORMAT_VIDEO_FRAMERATE);
        p.choice(CHOICE_RANGE, TYPE_FRACTION, 2, &[60, 1, 0, 1, 1000, 1]);
    })
}

/// Whether to ask the compositor for DMA-BUF frames instead of shared memory.
///
/// Off unless `OPENUUYC_CAPTURE_DMABUF=1`. The request is also gated on the
/// VA-API encoder being available, because that is the only encoder that can
/// read a DMA-BUF: asking for one without it leaves the capture with no
/// readable frames at all.
fn dmabuf_requested() -> bool {
    if !std::env::var("OPENUUYC_CAPTURE_DMABUF").is_ok_and(|value| value != "0") {
        return false;
    }
    super::super::encoder::Encoder::vaapi_accepts(crate::media::encoding::Format::AVC)
}

/// The buffer types to offer, which must agree with the negotiated format.
///
/// libpipewire collapses a multi-bit mask to a single type, and `MemPtr` wins
/// over everything else: offering it alongside `DmaBuf` makes the link
/// negotiate CPU-mapped memory while the portal has already committed to a
/// DMA-BUF, and buffer allocation then fails. So a DMA-BUF format asks for
/// DMA-BUF alone.
fn buffers(dmabuf: bool) -> Pod {
    Pod::object(OBJECT_PARAM_BUFFERS, PARAM_BUFFERS, |p| {
        p.key(BUFFERS_DATA_TYPE);
        // A flags choice holds one value: the mask. The count `SPA_CHOICE_FLAGS`
        // writes is not part of the pod -- `SPA_POD_CHOICE_N_VALUES` derives it
        // from the body size, which `choice` sets from the value count here.
        let mask = if dmabuf {
            1 << DATA_MEM_DMABUF
        } else {
            (1 << DATA_MEM_PTR) | (1 << DATA_MEM_FD)
        };
        p.choice(CHOICE_FLAGS, TYPE_INT, 1, &[mask]);
    })
}

fn crop_meta() -> Pod {
    Pod::object(OBJECT_PARAM_META, PARAM_META, |p| {
        p.key(META_TYPE);
        p.value(TYPE_ID, &[META_VIDEO_CROP]);
        p.key(META_SIZE);
        p.value(TYPE_INT, &[size_of::<SpaMetaRegion>() as u32]);
    })
}

/// The negotiated format from a Format object.
///
/// `modifier` is present only when the compositor agreed to DMA-BUF; a shared
/// memory stream leaves it `None`. It is what decides which path the frame
/// takes, not the format code, because a DRM fourcc can also arrive over
/// shared memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NegotiatedFormat {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub modifier: Option<u64>,
}

/// The negotiated format from a Format object.
///
/// # Safety
/// `pod` must point at a valid SPA pod.
unsafe fn parse_format(pod: *const c_void) -> Option<NegotiatedFormat> {
    let read =
        |offset: usize| unsafe { pod.cast::<u8>().add(offset).cast::<u32>().read_unaligned() };
    let read64 =
        |offset: usize| unsafe { pod.cast::<u8>().add(offset).cast::<u64>().read_unaligned() };
    let size = read(0) as usize;
    if read(4) != TYPE_OBJECT || size < 8 {
        return None;
    }
    let end = 8 + size;
    let mut offset = 16;
    let (mut format, mut width, mut height) = (None, None, None);
    let mut modifier = None;
    while offset + 16 <= end {
        let key = read(offset);
        let value_size = read(offset + 8) as usize;
        let mut value_type = read(offset + 12);
        let mut value = offset + 16;
        if value_type == TYPE_CHOICE {
            // A fixed property may still come wrapped in a None choice; its
            // first element is the value.
            value_type = read(value + 12);
            value += 16;
        }
        match (key, value_type) {
            (FORMAT_VIDEO_FORMAT, TYPE_ID) => format = Some(read(value)),
            (FORMAT_VIDEO_MODIFIER, TYPE_LONG) => modifier = Some(read64(value)),
            (FORMAT_VIDEO_SIZE, TYPE_RECTANGLE) => {
                width = Some(read(value));
                height = Some(read(value + 4));
            }
            _ => {}
        }
        offset += 16 + value_size.next_multiple_of(8);
    }
    Some(NegotiatedFormat {
        format: format?,
        width: width?,
        height: height?,
        modifier,
    })
}

// --- Stream ----------------------------------------------------------------------

/// One captured picture as the stream delivered it.
pub(super) enum Picture {
    /// Tightly packed BGRA in system memory, `width * height * 4` bytes.
    Shm {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// A DMA-BUF the encoder imports and converts on the GPU.
    Dmabuf(DmabufPicture),
}

impl Picture {
    pub(super) fn width(&self) -> u32 {
        match self {
            Self::Shm { width, .. } => *width,
            Self::Dmabuf(picture) => picture.width,
        }
    }

    pub(super) fn height(&self) -> u32 {
        match self {
            Self::Shm { height, .. } => *height,
            Self::Dmabuf(picture) => picture.height,
        }
    }

    /// Whether this picture keeps the PipeWire buffer out of the pool.
    ///
    /// A DMA-BUF is read later by the encoder, so its buffer must not be
    /// recycled yet; the shared-memory copy is already made and the buffer can
    /// go straight back.
    fn holds_buffer(&self) -> bool {
        matches!(self, Self::Dmabuf(_))
    }
}

/// A picture the compositor handed over as a file descriptor.
pub(crate) struct DmabufPicture {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc of the buffer, which is what VA-API imports it as.
    pub fourcc: u32,
    pub modifier: u64,
    /// Plane descriptors: `(fd, offset, pitch)`.
    pub planes: Vec<(RawFd, u32, u32)>,
    /// Returns the buffer to the compositor once the picture is dropped.
    _lease: BufferLease,
}

impl DmabufPicture {
    /// The extent of the buffer behind the planes, in bytes.
    ///
    /// A compositor leaves `spa_data.maxsize` at zero for DMA-BUFs -- its own
    /// clients read `chunk->size` instead -- so the driver has to be told how
    /// much memory there is. A single row is not enough: the import would then
    /// cover one line of the picture.
    pub(crate) fn buffer_size(&self) -> u32 {
        self.planes
            .iter()
            .map(|(_, offset, pitch)| offset.saturating_add(pitch.saturating_mul(self.height)))
            .max()
            .unwrap_or(0)
    }
}

/// Keeps a PipeWire buffer out of the compositor's pool until it is dropped.
///
/// `pw_stream_queue_buffer` is documented RT-safe and callable from a thread
/// other than the one that dequeued, which is what lets the encoder hold the
/// buffer across the conversion instead of copying the frame out first.
///
/// It holds the stream's [`Session`] rather than the raw handles, so a picture
/// that outlives the [`Stream`] -- a cached one, or one kept across a backend
/// change -- still returns its buffer to a live stream instead of a freed one.
struct BufferLease {
    session: Arc<Session>,
    buffer: *mut PwBuffer,
}

// SAFETY: the session owns the stream and outlives every lease it hands out;
// `pw_stream_queue_buffer` is the documented way to return a buffer and is safe
// to call from another thread.
unsafe impl Send for BufferLease {}

impl Drop for BufferLease {
    fn drop(&mut self) {
        unsafe { (self.session.api.stream_queue_buffer)(self.session.stream, self.buffer) };
    }
}

#[derive(Default)]
struct State {
    picture: Option<Picture>,
    sequence: u64,
    format: Option<NegotiatedFormat>,
    streaming: bool,
    error: Option<String>,
    /// The buffer kind the stream last asked for: `Some(true)` for DMA-BUF,
    /// `Some(false)` for shared memory, `None` before the first request.
    buffers_requested: Option<bool>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// The stream's handles, so a callback can hand out a lease that keeps them
    /// alive. Weak because the session owns the loop thread that runs the
    /// callbacks: a strong reference here would be a cycle.
    session: Mutex<std::sync::Weak<Session>>,
}

/// What the loop thread's callbacks reach through their `data` pointer.
struct Callbacks {
    api: &'static Api,
    stream: *mut c_void,
    shared: Arc<Shared>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let mut guard = lock(&callbacks.shared.state);
    match state {
        STATE_STREAMING => guard.streaming = true,
        STATE_ERROR => {
            let text = if error.is_null() {
                "未知错误".to_owned()
            } else {
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            guard.error = Some(format!("PipeWire 画面流出错：{text}"));
        }
        STATE_UNCONNECTED if guard.streaming => {
            guard.error = Some("屏幕共享已结束".into());
        }
        _ => {}
    }
    drop(guard);
    callbacks.shared.changed.notify_all();
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    if param.is_null() || id != PARAM_FORMAT {
        return;
    }
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let format = unsafe { parse_format(param) };
    let mut state = lock(&callbacks.shared.state);
    state.format = format;
    // A modifier that was actually fixated means a DMA-BUF; the portal writes
    // `DRM_FORMAT_MOD_INVALID` when it falls back to shared memory, and an
    // unfixed modifier says nothing about the layout.
    let dmabuf = format
        .and_then(|format| format.modifier)
        .is_some_and(|modifier| modifier != DRM_MOD_INVALID);
    // The buffer types are only re-sent when the kind changes. Sending them on
    // every format change makes the portal re-fixate the modifier, and when it
    // cannot it falls back to shared memory, reports that as another format
    // change, and the two sides loop: the portal logged over a thousand fixate
    // attempts in a few seconds and the stream never settled.
    if state.buffers_requested == Some(dmabuf) {
        drop(state);
        tracing::debug!(?format, dmabuf, "PipeWire screencast format renegotiated");
        return;
    }
    state.buffers_requested = Some(dmabuf);
    drop(state);
    tracing::debug!(?format, dmabuf, "PipeWire screencast format negotiated");
    let buffers = buffers(dmabuf).aligned();
    let meta = crop_meta().aligned();
    let mut params = [buffers.as_ptr().cast::<c_void>(), meta.as_ptr().cast()];
    unsafe {
        (callbacks.api.stream_update_params)(callbacks.stream, params.as_mut_ptr(), 2);
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let api = callbacks.api;
    // Only the newest buffer matters; return any older ones unread.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let buffer = unsafe { (api.stream_dequeue_buffer)(callbacks.stream) };
        if buffer.is_null() {
            break;
        }
        if !newest.is_null() {
            unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
        }
        newest = buffer;
    }
    if newest.is_null() {
        return;
    }
    let format = lock(&callbacks.shared.state).format;
    // A DMA-BUF picture keeps the buffer until the encoder is done with it, so
    // the lease carries the queue call; everything else is read here and the
    // buffer goes straight back.
    let picture = format.and_then(|format| unsafe {
        read_picture(
            api,
            callbacks.stream,
            &callbacks.shared,
            (*newest).buffer,
            format,
        )
    });
    if !picture
        .as_ref()
        .is_some_and(|picture| picture.holds_buffer())
    {
        unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
    }
    if let Some(picture) = picture {
        let mut state = lock(&callbacks.shared.state);
        state.picture = Some(picture);
        state.sequence += 1;
        drop(state);
        callbacks.shared.changed.notify_all();
    }
}

/// Copy a buffer's pixels out as tightly packed BGRA, or hand a DMA-BUF over.
///
/// # Safety
/// `buffer` must be a buffer the stream just dequeued.
unsafe fn read_picture(
    api: &'static Api,
    stream: *mut c_void,
    shared: &Shared,
    buffer: *mut SpaBuffer,
    negotiated: NegotiatedFormat,
) -> Option<Picture> {
    let NegotiatedFormat {
        format,
        width,
        height,
        modifier,
    } = negotiated;
    let buffer_ref = unsafe { &*buffer };
    if buffer_ref.n_datas == 0 || buffer_ref.datas.is_null() {
        return None;
    }
    // The buffer's own type decides, not the format's modifier. The portal
    // keeps the modifier property in the format even after it fails to allocate
    // a DMA-BUF and falls back to shared memory, so a `modifier` alone says
    // nothing about what the buffer actually is; reading the fallback as a
    // DMA-BUF imports the wrong memory and produces a blank picture.
    //
    // A delivered buffer carries the single `spa_data_type`, not the bitmask
    // the request used, so this compares the enum value directly.
    let kind = unsafe { (*buffer_ref.datas).kind };
    if kind == DATA_MEM_DMABUF {
        let session = shared
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()?;
        return unsafe { read_dmabuf(&session, buffer, format, width, height, modifier) };
    }
    let buffer = buffer_ref;
    let data = unsafe { &*buffer.datas };
    if data.data.is_null() || data.chunk.is_null() {
        // A DMA-BUF was not mapped for the CPU. Asking the portal for one does
        // not help: Hyprland's ScreenCast offers only MemFd, so this stays the
        // path a GPU encoder would need to import through PipeWire directly.
        tracing::debug!(
            data_type = data.kind,
            "PipeWire buffer is not CPU-mappable"
        );
        return None;
    }
    let chunk = unsafe { &*data.chunk };
    // The type the portal actually chose, which is the only way to tell whether
    // a DMA-BUF request was honoured or quietly answered with shared memory.
    tracing::debug!(
        data_type = data.kind,
        maxsize = data.maxsize,
        "PipeWire buffer delivered"
    );
    let stride = if chunk.stride > 0 {
        chunk.stride as usize
    } else {
        width as usize * 4
    };
    let (mut x, mut y, mut w, mut h) = (0usize, 0usize, width as usize, height as usize);
    for index in 0..buffer.n_metas as usize {
        let meta = unsafe { &*buffer.metas.add(index) };
        if meta.kind == META_VIDEO_CROP
            && meta.size as usize >= size_of::<SpaMetaRegion>()
            && !meta.data.is_null()
        {
            let region = unsafe { &*meta.data.cast::<SpaMetaRegion>() };
            if region.width > 0 && region.height > 0 {
                x = region.x.max(0) as usize;
                y = region.y.max(0) as usize;
                w = (region.width as usize).min(width as usize - x.min(width as usize));
                h = (region.height as usize).min(height as usize - y.min(height as usize));
            }
        }
    }
    let base = (chunk.offset % data.maxsize.max(1)) as usize;
    let needed = base + (y + h).saturating_sub(1) * stride + (x + w) * 4;
    if w == 0 || h == 0 || needed > data.maxsize as usize {
        return None;
    }
    let source =
        unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), data.maxsize as usize) };
    // Only the encoders read these pixels, and both convert with the colour
    // channels alone: the alpha byte is never looked at. Writing it would cost
    // a pass over the frame for nothing, so the buffer is left as the
    // compositor sent it.
    let swap = matches!(format, VIDEO_RGBX | VIDEO_RGBA);
    let mut pixels = vec![0u8; w * h * 4];
    if !swap {
        // The common case is already BGRX, so each row is a straight copy.
        for row in 0..h {
            let start = base + (y + row) * stride + x * 4;
            pixels[row * w * 4..(row + 1) * w * 4]
                .copy_from_slice(&source[start..start + w * 4]);
        }
    } else {
        // Swap red and blue while copying, so the result is BGRA either way.
        for row in 0..h {
            let start = base + (y + row) * stride + x * 4;
            let source_row = &source[start..start + w * 4];
            let target = &mut pixels[row * w * 4..(row + 1) * w * 4];
            for (texel, out) in source_row.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
                out[0] = texel[2];
                out[1] = texel[1];
                out[2] = texel[0];
                out[3] = texel[3];
            }
        }
    }
    Some(Picture::Shm {
        width: w as u32,
        height: h as u32,
        pixels,
    })
}

/// The DRM fourcc a negotiated `spa_video_format` arrives as.
///
/// The portal maps the compositor's DRM fourcc to a SPA enum on the way out
/// (`pwFromDrmFourcc`), so this is that mapping read backwards. The modifier
/// rides along and says how the buffer is tiled.
fn drm_fourcc(format: u32) -> Option<u32> {
    Some(match format {
        VIDEO_BGRA => u32::from_le_bytes(*b"AR24"),
        VIDEO_BGRX => u32::from_le_bytes(*b"XR24"),
        VIDEO_RGBA => u32::from_le_bytes(*b"AB24"),
        VIDEO_RGBX => u32::from_le_bytes(*b"XB24"),
        _ => return None,
    })
}

/// Hand a compositor DMA-BUF to the encoder without touching the pixels.
///
/// # Safety
/// `buffer` must be a buffer the stream just dequeued; the returned picture
/// keeps it out of the pool until it is dropped.
unsafe fn read_dmabuf(
    session: &Arc<Session>,
    buffer: *mut SpaBuffer,
    format: u32,
    width: u32,
    height: u32,
    modifier: Option<u64>,
) -> Option<Picture> {
    let fourcc = drm_fourcc(format)?;
    // A modifier the compositor never fixated tells the importer nothing about
    // the layout, and guessing wrong reads the wrong bytes. `LINEAR` is the
    // safe reading only when the portal says so; `INVALID` means it did not.
    let modifier = match modifier {
        Some(modifier) if modifier != DRM_MOD_INVALID => modifier,
        _ => {
            tracing::warn!(format, "DMA-BUF 未提供有效 modifier，跳过该画面");
            return None;
        }
    };
    let buffer_ref = unsafe { &*buffer };
    if buffer_ref.n_datas == 0 || buffer_ref.datas.is_null() {
        return None;
    }
    let mut planes = Vec::with_capacity(buffer_ref.n_datas as usize);
    for index in 0..buffer_ref.n_datas as usize {
        let data = unsafe { &*buffer_ref.datas.add(index) };
        if data.fd < 0 || data.chunk.is_null() {
            return None;
        }
        let chunk = unsafe { &*data.chunk };
        let pitch = if chunk.stride > 0 {
            chunk.stride as u32
        } else {
            width * 4
        };
        planes.push((data.fd as RawFd, chunk.offset, pitch));
    }
    if planes.is_empty() {
        return None;
    }
    tracing::debug!(
        fourcc,
        width,
        height,
        modifier,
        planes = planes.len(),
        "DMA-BUF 画面已交付"
    );
    Some(Picture::Dmabuf(DmabufPicture {
        width,
        height,
        fourcc,
        modifier,
        planes,
        _lease: BufferLease {
            session: Arc::clone(session),
            buffer: buffer.cast(),
        },
    }))
}

/// A video stream from one PipeWire node, on its own loop thread.
pub(super) struct Stream {
    session: Arc<Session>,
    shared: Arc<Shared>,
    delivered: u64,
}

/// The stream's PipeWire handles, shared with any buffer leases still out.
///
/// Teardown lives here rather than on [`Stream`] so that a picture holding a
/// buffer keeps the stream alive until the buffer is returned. Destroying the
/// stream while a lease is out would leave the lease calling into freed
/// memory.
struct Session {
    api: &'static Api,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    _hook: Box<SpaHook>,
    _events: Box<StreamEvents>,
    callbacks: *mut Callbacks,
}

// The raw handles are only used under the thread loop's lock or after it stopped.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        let api = self.api;
        // SAFETY: teardown in reverse order of setup; the loop is stopped
        // before the callback data it could still call into is freed.
        unsafe {
            (api.thread_loop_lock)(self.thread_loop);
            if !self.stream.is_null() {
                (api.stream_disconnect)(self.stream);
                (api.stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (api.core_disconnect)(self.core);
            }
            (api.thread_loop_unlock)(self.thread_loop);
            (api.thread_loop_stop)(self.thread_loop);
            (api.context_destroy)(self.context);
            (api.thread_loop_destroy)(self.thread_loop);
            if !self.callbacks.is_null() {
                drop(Box::from_raw(self.callbacks));
            }
        }
    }
}

impl Stream {
    /// Connect to `node` on the PipeWire remote behind `fd` (from the portal).
    pub fn connect(fd: std::os::fd::OwnedFd, node: u32) -> Result<Self> {
        use std::os::fd::IntoRawFd;
        let api = api()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            session: Mutex::new(std::sync::Weak::new()),
        });
        let name = CString::new("openuuyc-capture")?;
        // SAFETY: the PipeWire calls follow the documented setup sequence; every
        // handle is checked and owned by the returned value, which tears them
        // down in reverse order.
        unsafe {
            let thread_loop = (api.thread_loop_new)(name.as_ptr(), std::ptr::null());
            ensure!(!thread_loop.is_null(), "无法创建 PipeWire 线程");
            let context = (api.context_new)(
                (api.thread_loop_get_loop)(thread_loop),
                std::ptr::null_mut(),
                0,
            );
            if context.is_null() {
                (api.thread_loop_destroy)(thread_loop);
                bail!("无法创建 PipeWire 上下文");
            }
            let mut this = Session {
                api,
                thread_loop,
                context,
                core: std::ptr::null_mut(),
                stream: std::ptr::null_mut(),
                _hook: Box::new(std::mem::zeroed()),
                _events: Box::new(StreamEvents {
                    version: STREAM_EVENTS_VERSION,
                    destroy: None,
                    state_changed: Some(on_state_changed),
                    control_info: None,
                    io_changed: None,
                    param_changed: Some(on_param_changed),
                    add_buffer: None,
                    remove_buffer: None,
                    process: Some(on_process),
                    drained: None,
                    command: None,
                    trigger_done: None,
                }),
                callbacks: std::ptr::null_mut(),
            };
            ensure!(
                (api.thread_loop_start)(thread_loop) >= 0,
                "无法启动 PipeWire 线程"
            );
            (api.thread_loop_lock)(thread_loop);
            let result = (|| -> Result<()> {
                this.core =
                    (api.context_connect_fd)(context, fd.into_raw_fd(), std::ptr::null_mut(), 0);
                ensure!(!this.core.is_null(), "无法连接屏幕共享的 PipeWire 远端");
                let key = |s: &str| CString::new(s).unwrap();
                let (media_type, video, category, capture, role, screen) = (
                    key("media.type"),
                    key("Video"),
                    key("media.category"),
                    key("Capture"),
                    key("media.role"),
                    key("Screen"),
                );
                let props = (api.properties_new)(
                    media_type.as_ptr(),
                    video.as_ptr(),
                    category.as_ptr(),
                    capture.as_ptr(),
                    role.as_ptr(),
                    screen.as_ptr(),
                    std::ptr::null::<c_char>(),
                );
                this.stream = (api.stream_new)(this.core, name.as_ptr(), props);
                ensure!(!this.stream.is_null(), "无法创建 PipeWire 画面流");
                this.callbacks = Box::into_raw(Box::new(Callbacks {
                    api,
                    stream: this.stream,
                    shared: Arc::clone(&shared),
                }));
                (api.stream_add_listener)(
                    this.stream,
                    &mut *this._hook,
                    &*this._events,
                    this.callbacks.cast(),
                );
                // Only the format goes in with the connection. The buffer
                // types follow from the format `on_param_changed` reports, and
                // sending them before that is known risks asking for a DMA-BUF
                // the portal then cannot fixate.
                let format = enum_format().aligned();
                let mut params = [format.as_ptr().cast::<c_void>()];
                let connected = (api.stream_connect)(
                    this.stream,
                    DIRECTION_INPUT,
                    node,
                    FLAG_AUTOCONNECT | FLAG_MAP_BUFFERS,
                    params.as_mut_ptr(),
                    params.len() as u32,
                );
                ensure!(connected >= 0, "连接 PipeWire 画面流失败：{connected}");
                Ok(())
            })();
            (api.thread_loop_unlock)(thread_loop);
            result.map(|()| {
                let session = Arc::new(this);
                *shared.session.lock().unwrap_or_else(|e| e.into_inner()) =
                    Arc::downgrade(&session);
                Stream {
                    session,
                    shared,
                    delivered: 0,
                }
            })
        }
    }

    /// The next picture newer than the last one returned, waiting up to
    /// `timeout`. `None` when nothing new arrived.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(error) = &state.error {
                bail!("{error}");
            }
            if state.sequence != self.delivered
                && let Some(picture) = state.picture.take()
            {
                self.delivered = state.sequence;
                return Ok(Some(picture));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Teardown happens when the last buffer lease goes, which is in
        // `Session::drop`.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_parses_back() {
        // A fixed format written the way enum_format writes a choice's first
        // element must be read by parse_format.
        let pod = Pod::object(OBJECT_FORMAT, PARAM_FORMAT, |p| {
            p.key(FORMAT_MEDIA_TYPE);
            p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
            p.key(FORMAT_VIDEO_FORMAT);
            p.value(TYPE_ID, &[VIDEO_BGRX]);
            p.key(FORMAT_VIDEO_SIZE);
            p.value(TYPE_RECTANGLE, &[2560, 1440]);
        })
        .aligned();
        assert_eq!(
            unsafe { parse_format(pod.as_ptr().cast()) },
            Some(NegotiatedFormat {
                format: VIDEO_BGRX,
                width: 2560,
                height: 1440,
                modifier: None,
            })
        );
        let choices = enum_format().aligned();
        let size = unsafe { choices.as_ptr().cast::<u32>().read() } as usize;
        assert_eq!((size + 8) % 8, 0);
    }

    #[test]
    fn buffers_offers_dmabuf_alone_in_one_flags_choice() {
        // The portal only offers DMA-BUF when the mask reaches it intact, and
        // the value count is derived from the body size rather than stored, so
        // a wrong word count silently produces a different number of values.
        let pod = buffers(true).aligned();
        let words: Vec<u32> = (0..14)
            .map(|index| unsafe { pod.as_ptr().cast::<u32>().add(index).read() })
            .collect();
        // [size, TYPE_OBJECT, ParamBuffers, PARAM_BUFFERS, key, pad,
        //  choice size, TYPE_CHOICE, CHOICE_FLAGS, 0, child size, TYPE_INT, mask]
        assert_eq!(words[1], TYPE_OBJECT);
        assert_eq!(words[2], OBJECT_PARAM_BUFFERS);
        assert_eq!(words[3], PARAM_BUFFERS);
        assert_eq!(words[4], BUFFERS_DATA_TYPE);
        assert_eq!(words[7], TYPE_CHOICE);
        assert_eq!(words[8], CHOICE_FLAGS);
        assert_eq!(words[11], TYPE_INT);
        // DMA-BUF alone: `MemPtr` alongside it would win the collapse inside
        // libpipewire and make allocation fail against a DMA-BUF format.
        assert_eq!(words[12], 1 << DATA_MEM_DMABUF);
        // The shared-memory offer still names a CPU-readable type.
        let shm = buffers(false).aligned();
        let mask = unsafe { shm.as_ptr().cast::<u32>().add(12).read() };
        assert_eq!(mask, (1 << DATA_MEM_PTR) | (1 << DATA_MEM_FD));
    }
}
