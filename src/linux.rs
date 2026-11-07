//! The only unsafe code here wraps the private GPU backend and lease ABI.
//! Conversion stays queued on the GPU after `process` returns: the output
//! carries an acquire fence, and the input stays retained until that fence
//! signals. No processing thread waits for a game-contended GPU to finish.
use crate::Settings;
use amaterasu_media::*;
use amaterasu_module_sdk::{
    abi::*,
    processing::{GpuFrame, GpuFrameProcessor, GpuOutputFrame, RetainedGpuInput},
};
use std::os::unix::ffi::OsStrExt;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::{CStr, CString, c_char, c_void},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    rc::Rc,
};

/// Converted inputs still awaiting their GPU reads. Normally the oldest has
/// already signalled when the next frame arrives; past this bound the
/// provider's release waits for it, so storage pinning stays bounded.
const MAX_PENDING_INPUTS: usize = 2;

unsafe extern "C" {
    fn am_rtx_open(
        root: *const c_char,
        device: i32,
        width: u32,
        height: u32,
        parameters: *const u32,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn am_rtx_close(context: *mut c_void);
    fn am_rtx_configure(
        context: *mut c_void,
        parameters: *const u32,
        enabled: bool,
        sdr_brightness: u32,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn am_rtx_output_open(
        context: *mut c_void,
        frame: *mut DmaBufFrame,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn am_rtx_output_close(context: *mut c_void, output: *mut c_void);
    fn am_rtx_process(
        context: *mut c_void,
        frame: *const DmaBufFrame,
        output: *mut c_void,
        completion_fence: *mut i32,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn am_rtx_wait_fence(fd: i32) -> i32;
    fn am_rtx_context_priority(context: *mut c_void) -> i32;
}

struct NativeSession(NonNull<c_void>);
impl Drop for NativeSession {
    fn drop(&mut self) {
        // SAFETY: all output surfaces hold an Rc to this session and finish
        // their queued writes when closed; closing drains the CUDA stream.
        unsafe { am_rtx_close(self.0.as_ptr()) };
    }
}
struct Slot {
    session: Rc<NativeSession>,
    output: NonNull<c_void>,
    frame: DmaBufFrame,
    busy: Cell<bool>,
    /// Signals when the latest conversion into this slot has completed. Its
    /// descriptor is lent as the output's acquire fence, so it is replaced
    /// only while the slot has no outstanding lease reference.
    acquire_fence: RefCell<Option<OwnedFd>>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        assert!(
            !self.busy.get(),
            "HDR output destroyed with live references"
        );
        // SAFETY: this owns one output; session outlives the call and all work
        // has retired before busy clears. No descriptors cross thread boundaries.
        unsafe { am_rtx_output_close(self.session.0.as_ptr(), self.output.as_ptr()) };
    }
}
struct Stream {
    slots: Vec<Rc<Slot>>,
    cached: Option<CachedOutput>,
    pending_inputs: VecDeque<PendingInput>,
    width: u32,
    height: u32,
    enabled: bool,
    reported_enabled: Option<bool>,
}
struct Lease {
    slot: Rc<Slot>,
    references: Cell<u32>,
}

/// One reference keeps the most recent immutable result available for capture
/// retries and keepalives. The cache owns a regular output lease, so downstream
/// references continue to protect the same slot after the cache is replaced.
struct CachedOutput {
    lease: DmaBufFrameLease,
    input_sequence: u64,
}

impl CachedOutput {
    fn deliver(&self, input: &DmaBufFrame) -> Result<GpuOutputFrame, Status> {
        let mut lease = self.lease;
        lease.frame.sequence = input.sequence;
        lease.frame.timestamp_us = input.timestamp_us;
        // SAFETY: the cache owns a live reference throughout this retain.
        let status = unsafe { retain(lease.context) };
        if status != STATUS_OK {
            return Err(status);
        }
        // SAFETY: the successful retain transfers one independent reference;
        // the cache and any downstream owners keep the slot immutable.
        unsafe { GpuOutputFrame::from_owned_lease(lease) }
    }
}

impl Drop for CachedOutput {
    fn drop(&mut self) {
        // SAFETY: constructing the cache transferred exactly one reference.
        unsafe { release(self.lease.context, -1) };
    }
}

/// A borrowed input whose queued GPU reads may still be running. The fence
/// signals after the conversion that read it, so release hands the fence to
/// the input provider, which waits on it before reusing the storage.
struct PendingInput {
    input: Option<RetainedGpuInput>,
    completion: Option<OwnedFd>,
}

impl PendingInput {
    fn completed(&self) -> bool {
        self.completion.as_ref().is_none_or(fence_signalled)
    }
}

impl Drop for PendingInput {
    fn drop(&mut self) {
        if let (Some(input), Some(completion)) = (self.input.take(), self.completion.take()) {
            input.release_after(completion);
        }
    }
}

/// Polls without blocking. An errored fence also reports readable; the input
/// provider's release inspects its status and quarantines storage on failure.
fn fence_signalled(fence: &OwnedFd) -> bool {
    let mut poller = libc::pollfd {
        fd: fence.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live pollfd whose descriptor is held open by `fence`.
    let ready = unsafe { libc::poll(&mut poller, 1, 0) };
    ready == 1 && poller.revents & (libc::POLLIN | libc::POLLERR) != 0
}

unsafe extern "C" fn retain(context: *mut c_void) -> Status {
    // SAFETY: each live descriptor pins its boxed lease through all references.
    let lease = unsafe { &*context.cast::<Lease>() };
    if lease.references.get() >= 64 {
        return MEDIA_BUSY;
    }
    lease.references.set(lease.references.get() + 1);
    STATUS_OK
}
unsafe extern "C" fn release(context: *mut c_void, fence: i32) {
    if fence >= 0 {
        // SAFETY: the ABI transfers this fd. Validate/wait before freeing any
        // immutable pixels; failed completion must never allow reuse or unload.
        let status = unsafe { am_rtx_wait_fence(fence) };
        unsafe {
            libc::close(fence);
        }
        if status != 0 {
            std::process::abort();
        }
    }
    // SAFETY: one retained reference is transferred back by this call.
    let lease = unsafe { &*context.cast::<Lease>() };
    let Some(remaining) = lease.references.get().checked_sub(1) else {
        std::process::abort();
    };
    if remaining == 0 {
        // SAFETY: the final reference uniquely owns the box. Completion was
        // established above, so its pool slot can now be reused.
        let lease = unsafe { Box::from_raw(context.cast::<Lease>()) };
        lease.slot.busy.set(false);
    } else {
        lease.references.set(remaining);
    }
}

/// One queued conversion: the output's acquire fence and, when the input was
/// retained, that input with its own duplicate of the completion fence.
struct QueuedConversion {
    fence: OwnedFd,
    input: Option<PendingInput>,
}

const EGL_CONTEXT_PRIORITY_HIGH_IMG: i32 = 0x3101;
const EGL_CONTEXT_PRIORITY_MEDIUM_IMG: i32 = 0x3102;
const EGL_CONTEXT_PRIORITY_LOW_IMG: i32 = 0x3103;

/// Reports the GPU scheduling priority the driver granted the effect context.
fn report_context_priority(
    session: &NativeSession,
    logger: Option<&amaterasu_module_sdk::LogEmitter>,
) {
    let Some(logger) = logger else {
        return;
    };
    // SAFETY: the session is live for this synchronous query.
    let priority = match unsafe { am_rtx_context_priority(session.0.as_ptr()) } {
        EGL_CONTEXT_PRIORITY_HIGH_IMG => "high",
        EGL_CONTEXT_PRIORITY_MEDIUM_IMG => "medium",
        EGL_CONTEXT_PRIORITY_LOW_IMG => "low",
        _ => "unavailable",
    };
    let message = serde_json::json!({
        "event": "rtx_hdr_context",
        "gpu_priority": priority,
        "requested_gpu_priority": "high",
    })
    .to_string();
    let _ = logger.emit(amaterasu_module_sdk::LogLevel::Info, "rtx-hdr", &message);
}

fn backend_failure(
    error: &[c_char; 512],
    logger: Option<&amaterasu_module_sdk::LogEmitter>,
) -> Status {
    // SAFETY: the native adapter always NUL terminates this initialized buffer.
    let message = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
    if let Some(logger) = logger {
        let _ = logger.emit(amaterasu_module_sdk::LogLevel::Error, "rtx-hdr", &message);
    }
    MEDIA_BACKEND_ERROR
}

impl Stream {
    fn open(
        settings: &Settings,
        width: u32,
        height: u32,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<Self, Status> {
        let path = CString::new(settings.runtime_directory.as_os_str().as_bytes())
            .map_err(|_| STATUS_INVALID_ARGUMENT)?;
        let mut error = [0; 512];
        // SAFETY: path, parameters and writable error buffer outlive this call.
        // The backend transfers one owned session; it stores no Rust pointer.
        let raw = unsafe {
            am_rtx_open(
                path.as_ptr(),
                settings.device as i32,
                width,
                height,
                settings.parameters().as_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let session = Rc::new(NativeSession(
            NonNull::new(raw).ok_or_else(|| backend_failure(&error, logger))?,
        ));
        Self::configure_session(&session, settings, true, logger)?;
        let mut slots = Vec::new();
        for _ in 0..settings.output_buffers {
            // SAFETY: zeroed scalar descriptor is initialized by successful open.
            let mut frame: DmaBufFrame = unsafe { std::mem::zeroed() };
            let raw = unsafe {
                am_rtx_output_open(
                    session.0.as_ptr(),
                    &mut frame,
                    error.as_mut_ptr(),
                    error.len(),
                )
            };
            let output = NonNull::new(raw).ok_or_else(|| backend_failure(&error, logger))?;
            slots.push(Rc::new(Slot {
                session: session.clone(),
                output,
                frame,
                busy: Cell::new(false),
                acquire_fence: RefCell::new(None),
            }));
        }
        report_context_priority(&session, logger);
        Ok(Self {
            slots,
            cached: None,
            pending_inputs: VecDeque::new(),
            width,
            height,
            enabled: true,
            reported_enabled: None,
        })
    }

    fn configure_session(
        session: &NativeSession,
        settings: &Settings,
        enabled: bool,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<(), Status> {
        let mut error = [0; 512];
        // SAFETY: the session and parameter array remain valid for the synchronous call.
        let result = unsafe {
            am_rtx_configure(
                session.0.as_ptr(),
                settings.parameters().as_ptr(),
                enabled,
                settings.sdr_brightness,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result != 0 {
            return Err(backend_failure(&error, logger));
        }
        Ok(())
    }

    fn configure(
        &mut self,
        settings: &Settings,
        enabled: bool,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<(), Status> {
        Self::configure_session(&self.slots[0].session, settings, enabled, logger)?;
        // A live settings or foreground change can alter pixels even if the
        // capture provider keeps presenting the same source frame.
        self.cached = None;
        Ok(())
    }

    fn process(
        &mut self,
        source: &GpuFrame<'_>,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<GpuOutputFrame, Status> {
        let input = source.frame();
        if input.width != self.width || input.height != self.height {
            return Err(MEDIA_UNSUPPORTED);
        }
        self.retire_completed_inputs();
        if source.freshness() == CAPTURE_FRAME_CACHED_REUSE
            && let Some(cached) = self.cached.as_ref()
            && cached.input_sequence == input.sequence
        {
            return cached.deliver(input);
        }
        // Free the cache's reference before finding a slot. In-flight encoder
        // references still protect old output until they are retired.
        self.cached = None;
        let slot = self
            .slots
            .iter()
            .find(|slot| !slot.busy.get())
            .ok_or(MEDIA_BUSY)?
            .clone();
        // Keep the input readable after return while its conversion is still
        // queued. Without a reference, complete it before returning instead.
        let retained = source.retain().ok();
        let completion = Self::convert(&slot, input, logger, retained)?;
        slot.busy.set(true);
        let mut frame = DmaBufFrame {
            sequence: input.sequence,
            timestamp_us: input.timestamp_us,
            ..slot.frame
        };
        frame.planes[0].acquire_fence_fd = completion
            .as_ref()
            .map_or(-1, |pending| pending.fence.as_raw_fd());
        if let Some(pending) = completion {
            *slot.acquire_fence.borrow_mut() = Some(pending.fence);
            self.queue_pending_input(pending.input);
        }
        let lease = Box::new(Lease {
            slot,
            references: Cell::new(1),
        });
        self.cached = Some(CachedOutput {
            lease: DmaBufFrameLease {
                frame,
                context: Box::into_raw(lease).cast(),
                retain: Some(retain),
                release: Some(release),
            },
            input_sequence: input.sequence,
        });
        self.cached
            .as_ref()
            .expect("cache was populated")
            .deliver(input)
    }

    /// Queues one conversion into an idle `slot`. Returns its completion fence
    /// with the retained input it covers, or None once the work has finished.
    fn convert(
        slot: &Slot,
        input: &DmaBufFrame,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
        retained: Option<RetainedGpuInput>,
    ) -> Result<Option<QueuedConversion>, Status> {
        // The slot's previous fence must outlive no reader: it is idle, so no
        // lease still lends that descriptor. Its writes precede ours in order.
        slot.acquire_fence.borrow_mut().take();
        let mut error = [0; 512];
        let mut fence = -1;
        // SAFETY: the SDK lends immutable input pixels until this returns, and
        // `retained` extends that until the returned fence signals. This idle
        // slot is exclusively writable, with no downstream reference.
        let result = unsafe {
            am_rtx_process(
                slot.session.0.as_ptr(),
                input,
                slot.output.as_ptr(),
                &mut fence,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result != 0 {
            // The backend drained the GPU before reporting failure.
            if let Some(retained) = retained {
                // SAFETY: the failed call synchronized every queued read.
                unsafe { retained.release_completed() };
            }
            return Err(backend_failure(&error, logger));
        }
        if fence < 0 {
            if let Some(retained) = retained {
                // SAFETY: no fence means the backend completed synchronously.
                unsafe { retained.release_completed() };
            }
            return Ok(None);
        }
        // SAFETY: the backend transfers ownership of this new sync_file.
        let fence = unsafe { OwnedFd::from_raw_fd(fence) };
        let Some(retained) = retained else {
            // The borrowed input must be fully read before returning.
            Self::wait_input_reads(&fence);
            return Ok(Some(QueuedConversion { fence, input: None }));
        };
        let input_fence = match fence.try_clone() {
            Ok(input_fence) => input_fence,
            Err(_) => {
                Self::wait_input_reads(&fence);
                // SAFETY: the wait above established completion of every read.
                unsafe { retained.release_completed() };
                return Ok(Some(QueuedConversion { fence, input: None }));
            }
        };
        Ok(Some(QueuedConversion {
            fence,
            input: Some(PendingInput {
                input: Some(retained),
                completion: Some(input_fence),
            }),
        }))
    }

    fn wait_input_reads(fence: &OwnedFd) {
        // SAFETY: the borrowed descriptor stays open throughout the wait.
        if unsafe { am_rtx_wait_fence(fence.as_raw_fd()) } != 0 {
            // A failed GPU fence cannot prove the input reads finished, and
            // the input provider would reuse its storage after this returns.
            std::process::abort();
        }
    }

    fn queue_pending_input(&mut self, pending: Option<PendingInput>) {
        let Some(pending) = pending else {
            return;
        };
        self.pending_inputs.push_back(pending);
        while self.pending_inputs.len() > MAX_PENDING_INPUTS {
            // Dropping hands the fence to the input provider, which waits.
            self.pending_inputs.pop_front();
        }
    }

    fn retire_completed_inputs(&mut self) {
        while self
            .pending_inputs
            .front()
            .is_some_and(PendingInput::completed)
        {
            self.pending_inputs.pop_front();
        }
    }

    fn retire_inputs(&mut self) {
        // Each release passes its fence; the input provider waits before reuse.
        self.pending_inputs.clear();
    }

    fn assert_retired(&self) {
        // Video cancels downstream encoders before this stage. A reference here
        // means that barrier failed: do not unload a provider with live storage.
        if self.slots.iter().any(|slot| slot.busy.get()) {
            std::process::abort();
        }
    }
}

pub(crate) struct Processor {
    settings: Settings,
    stream_settings: BTreeMap<u64, Settings>,
    targets: BTreeMap<u64, Option<amaterasu_module_sdk::extension_api::LifecycleTarget>>,
    policy: Option<std::sync::Arc<std::sync::Mutex<crate::foreground_policy::ForegroundPolicy>>>,
    demand: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    demanded: BTreeSet<u64>,
    streams: BTreeMap<u64, Stream>,
    retired: BTreeSet<u64>,
    logger: Option<amaterasu_module_sdk::LogEmitter>,
}
impl Processor {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            stream_settings: BTreeMap::new(),
            targets: BTreeMap::new(),
            policy: None,
            demand: None,
            demanded: BTreeSet::new(),
            streams: BTreeMap::new(),
            retired: BTreeSet::new(),
            logger: None,
        }
    }
    pub fn with_policy(
        mut self,
        policy: std::sync::Arc<std::sync::Mutex<crate::foreground_policy::ForegroundPolicy>>,
        demand: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.policy = Some(policy);
        self.demand = Some(demand);
        self
    }
    fn set_demand(&mut self, stream: u64, wanted: bool) {
        let changed = if wanted {
            self.demanded.insert(stream)
        } else {
            self.demanded.remove(&stream)
        };
        if changed && let Some(counter) = &self.demand {
            if wanted {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    fn clear_demand(&mut self) {
        for stream in std::mem::take(&mut self.demanded) {
            if let Some(counter) = &self.demand {
                counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = stream;
        }
    }
    pub fn with_logger(mut self, logger: amaterasu_module_sdk::LogEmitter) -> Self {
        self.logger = Some(logger);
        self
    }
}
impl Drop for Processor {
    fn drop(&mut self) {
        self.clear_demand();
    }
}
impl GpuFrameProcessor for Processor {
    fn configure_stream(&mut self, stream: u64, json: &[u8]) -> Result<(), Status> {
        if self.retired.contains(&stream) {
            return Err(MEDIA_CLOSED);
        }
        if self.streams.contains_key(&stream) || self.stream_settings.contains_key(&stream) {
            return Err(MEDIA_BUSY);
        }
        if self.stream_settings.len() >= self.settings.max_streams || self.retired.len() >= 1024 {
            return Err(MEDIA_BUSY);
        }
        let controls: crate::settings::StreamSettings =
            serde_json::from_slice(json).map_err(|_| STATUS_INVALID_ARGUMENT)?;
        let settings = controls
            .apply(&self.settings)
            .map_err(|_| STATUS_INVALID_ARGUMENT)?;
        self.targets.insert(stream, controls.foreground_target);
        self.set_demand(
            stream,
            settings.enabled && (settings.foreground_only || settings.fullscreen_only),
        );
        self.stream_settings.insert(stream, settings);
        Ok(())
    }
    fn update_stream(&mut self, stream: u64, json: &[u8]) -> Result<(), Status> {
        if self.retired.contains(&stream) {
            return Err(MEDIA_CLOSED);
        }
        if !self.stream_settings.contains_key(&stream) && !self.streams.contains_key(&stream) {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let value: serde_json::Value =
            serde_json::from_slice(json).map_err(|_| STATUS_INVALID_ARGUMENT)?;
        let target_supplied = value.get("foreground_target").is_some();
        let controls: crate::settings::StreamSettings =
            serde_json::from_value(value).map_err(|_| STATUS_INVALID_ARGUMENT)?;
        let settings = controls
            .apply(&self.settings)
            .map_err(|_| STATUS_INVALID_ARGUMENT)?;
        if target_supplied
            && self.targets.get(&stream).cloned().flatten() != controls.foreground_target
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        if let Some(active) = self.streams.get_mut(&stream) {
            // Turning on a gated effect waits for the next foreground check;
            // turning it off takes effect immediately without reallocating.
            let enabled = settings.enabled
                && (active.enabled || (!settings.foreground_only && !settings.fullscreen_only));
            active.configure(&settings, enabled, self.logger.as_ref())?;
            active.enabled = enabled;
        }
        self.set_demand(
            stream,
            settings.enabled && (settings.foreground_only || settings.fullscreen_only),
        );
        self.stream_settings.insert(stream, settings);
        Ok(())
    }
    fn process(&mut self, stream: u64, input: GpuFrame<'_>) -> Result<GpuOutputFrame, Status> {
        if self.retired.contains(&stream) {
            return Err(MEDIA_CLOSED);
        }
        let frame = input.frame();
        if !supported_input(frame) {
            if let Some(logger) = &self.logger {
                let message = serde_json::json!({
                    "event": "rtx_hdr_input_rejected",
                    "stream": stream,
                    "width": frame.width,
                    "height": frame.height,
                    "color": frame.color,
                    "fourcc": frame.fourcc,
                    "planes": frame.plane_count,
                    "pitch": frame.planes[0].pitch,
                    "offset": frame.planes[0].offset,
                })
                .to_string();
                let _ = logger.emit(amaterasu_module_sdk::LogLevel::Error, "rtx-hdr", &message);
            }
            return Err(MEDIA_UNSUPPORTED);
        }
        if !self.streams.contains_key(&stream) {
            if self.streams.len() >= self.settings.max_streams || self.retired.len() >= 1024 {
                return Err(MEDIA_BUSY);
            }
            self.streams.insert(
                stream,
                Stream::open(
                    self.stream_settings.get(&stream).unwrap_or(&self.settings),
                    frame.width,
                    frame.height,
                    self.logger.as_ref(),
                )?,
            );
        }
        if !self.stream_settings.contains_key(&stream) {
            self.set_demand(
                stream,
                self.settings.enabled
                    && (self.settings.foreground_only || self.settings.fullscreen_only),
            );
        }
        let settings = self.stream_settings.get(&stream).unwrap_or(&self.settings);
        let enabled = settings.enabled
            && if !settings.foreground_only && !settings.fullscreen_only {
                true
            } else {
                self.policy.as_ref().is_some_and(|policy| {
                    policy.lock().ok().is_some_and(|policy| {
                        policy.enabled(
                            self.targets.get(&stream).and_then(Option::as_ref),
                            settings.foreground_only,
                            settings.fullscreen_only,
                            std::time::Instant::now(),
                        )
                    })
                })
            };
        let active = self.streams.get_mut(&stream).expect("stream was opened");
        if active.enabled != enabled {
            active.configure(settings, enabled, self.logger.as_ref())?;
            active.enabled = enabled;
        }
        let output = active.process(&input, self.logger.as_ref())?;
        if active.reported_enabled != Some(enabled) {
            if let Some(logger) = &self.logger {
                let message = serde_json::json!({
                    "event": "rtx_hdr_conversion",
                    "stream": stream,
                    "enabled": enabled,
                    "width": frame.width,
                    "height": frame.height,
                    "peak_nits": settings.peak_luminance,
                    "source": "srgb",
                    "output": "bt2020_pq",
                })
                .to_string();
                let _ = logger.emit(amaterasu_module_sdk::LogLevel::Info, "rtx-hdr", &message);
            }
            active.reported_enabled = Some(enabled);
        }
        Ok(output)
    }
    fn cancel(&mut self, stream: u64) {
        self.set_demand(stream, false);
        self.stream_settings.remove(&stream);
        self.targets.remove(&stream);
        if let Some(mut active) = self.streams.remove(&stream) {
            active.cached = None;
            active.retire_inputs();
            active.assert_retired();
        }
        if self.retired.len() < 1024 {
            self.retired.insert(stream);
        }
    }
    fn shutdown(&mut self) {
        for stream in self.streams.values_mut() {
            stream.cached = None;
            stream.retire_inputs();
            stream.assert_retired();
        }
        self.streams.clear();
        self.clear_demand();
        self.stream_settings.clear();
        self.targets.clear();
    }
    fn hdr_metadata(&self, stream: u64) -> Option<HdrMetadata> {
        Some(
            self.stream_settings
                .get(&stream)
                .unwrap_or(&self.settings)
                .hdr_metadata(),
        )
    }
}

/// SDR scanout may use either eight-bit or ten-bit packed RGB. EGL samples
/// both into the same RGBA8 model input; bit depth does not imply HDR transfer.
fn supported_input(frame: &DmaBufFrame) -> bool {
    frame.color == DMABUF_COLOR_SRGB
        && frame.plane_count == 1
        && matches!(
            frame.fourcc.to_le_bytes(),
            [b'X' | b'A', b'R' | b'B', b'2', b'4'] | [b'X' | b'A', b'R' | b'B', b'3', b'0']
        )
        && (16..=8192).contains(&frame.width)
        && (16..=8192).contains(&frame.height)
        && u64::from(frame.width) * u64::from(frame.height) * 4 <= MAX_FRAME_BYTES as u64
        && frame.planes[0].offset <= i32::MAX as u32
        && frame.planes[0].pitch <= i32::MAX as u32
}

#[cfg(test)]
mod tests;
