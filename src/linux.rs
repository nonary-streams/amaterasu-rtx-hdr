//! The only unsafe code here wraps the private GPU backend and lease ABI.
//! Conversion stays queued on the GPU after `process` returns: the output
//! carries an acquire fence, and the input stays retained until that fence
//! signals. No processing thread waits for a game-contended GPU to finish.
use crate::Settings;
use amaterasu_media::*;
use amaterasu_module_sdk::{
    abi::*,
    frame_processing::{
        FrameProcessor, ProcessingFrame, ResourceOutputFrame, RetainedProcessingInput,
    },
};
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::{CStr, CString, c_char, c_void},
    os::fd::{AsRawFd, OwnedFd},
    ptr::NonNull,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Converted inputs still awaiting their GPU reads. Normally the oldest has
/// already signalled when the next frame arrives; past this bound the
/// next call returns BUSY until a read retires; no input wait is introduced.
const MAX_PENDING_INPUTS: usize = 2;
static NATIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);

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
    fn am_rtx_finish(context: *mut c_void) -> i32;
    fn am_rtx_context_priority(context: *mut c_void) -> i32;
    fn am_rtx_take_timing(context: *mut c_void, totals: *mut GpuTiming);
}

/// Mirrors `rtx::TimingTotals`: GPU-clock stage times summed over a window.
#[repr(C)]
#[derive(Default, serde::Serialize)]
struct GpuTiming {
    samples: u64,
    gl_queue_us: u64,
    gl_queue_max_us: u64,
    gl_sample_us: u64,
    gl_sample_max_us: u64,
    cuda_pipeline_us: u64,
    cuda_pipeline_max_us: u64,
}
const _: () = assert!(std::mem::size_of::<GpuTiming>() == 7 * 8);

/// Matches the host's other stage windows so the logs line up.
const TIMING_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

struct NativeSession(NonNull<c_void>);
impl Drop for NativeSession {
    fn drop(&mut self) {
        // SAFETY: all output surfaces hold an Rc to this session and finish
        // their queued writes when closed; closing drains the CUDA stream.
        unsafe { am_rtx_close(self.0.as_ptr()) };
        NATIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Stream {
    session: Rc<NativeSession>,
    cuda_slots: Vec<Rc<resource::CudaSlot>>,
    cached_resource: Option<resource::CachedResource>,
    timing_reported: std::time::Instant,
    pending_inputs: VecDeque<PendingInput>,
    width: u32,
    height: u32,
    enabled: bool,
    reported_enabled: Option<bool>,
}
/// A borrowed input whose queued GPU reads may still be running. The fence
/// signals after the conversion that read it, so release hands the fence to
/// the input provider, which waits on it before reusing the storage.
enum ReadCompletion {
    Fence(OwnedFd),
    Cuda(Rc<resource::CudaSlot>),
}
struct PendingInput {
    input: Option<RetainedProcessingInput>,
    completion: ReadCompletion,
}
impl PendingInput {
    fn completed(&self) -> bool {
        match &self.completion {
            ReadCompletion::Fence(fence) => fence_signalled(fence),
            ReadCompletion::Cuda(slot) => slot.input_completed(),
        }
    }
}
impl Drop for PendingInput {
    fn drop(&mut self) {
        let Some(input) = self.input.take() else {
            return;
        };
        match &self.completion {
            ReadCompletion::Fence(completion) => {
                let sync = FrameSyncFile {
                    fd: completion.as_raw_fd(),
                };
                let descriptor = FrameDescriptor {
                    kind: FRAME_SYNC_SYNC_FILE,
                    version: 1,
                    size: std::mem::size_of::<FrameSyncFile>() as u32,
                    reserved: 0,
                    namespace: [0; 16],
                    data: std::ptr::from_ref(&sync).cast(),
                };
                // SAFETY: the dependency covers the queued GL sampling read;
                // its source duplicates it before this descriptor is dropped.
                unsafe { input.release_after(&descriptor) };
            }
            ReadCompletion::Cuda(slot) => {
                // Only completed polling or the native teardown drain may
                // retire CPU uploads. A missing proof keeps source storage pinned.
                if !slot.input_completed() {
                    input.quarantine();
                    slot.quarantine();
                    return;
                }
                unsafe { input.release_completed() };
                slot.release_input();
            }
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
        let raw = NonNull::new(raw).ok_or_else(|| backend_failure(&error, logger))?;
        NATIVE_SESSIONS.fetch_add(1, Ordering::AcqRel);
        let session = Rc::new(NativeSession(raw));
        Self::configure_session(&session, settings, true, logger)?;
        report_context_priority(&session, logger);
        Ok(Self {
            session,
            cuda_slots: Vec::new(),
            cached_resource: None,
            timing_reported: std::time::Instant::now(),
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
        Self::configure_session(&self.session, settings, enabled, logger)?;
        // A live settings or foreground change can alter pixels even if the
        // capture provider keeps presenting the same source frame.
        self.cached_resource = None;
        Ok(())
    }

    fn queue_pending_input(&mut self, pending: Option<PendingInput>) {
        let Some(pending) = pending else {
            return;
        };
        self.pending_inputs.push_back(pending);
        assert!(
            self.pending_inputs.len() <= MAX_PENDING_INPUTS,
            "input capacity reserved before enqueue"
        );
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

    /// Logs the window's GPU stage times. Reading never waits for the GPU.
    fn report_timing(&mut self, logger: Option<&amaterasu_module_sdk::LogEmitter>) {
        let Some(logger) = logger else {
            return;
        };
        if self.timing_reported.elapsed() < TIMING_WINDOW {
            return;
        }
        self.timing_reported = std::time::Instant::now();
        let mut timing = GpuTiming::default();
        // SAFETY: the session is live and the record has the C layout.
        unsafe { am_rtx_take_timing(self.session.0.as_ptr(), &mut timing) };
        let mut message = serde_json::to_value(&timing).unwrap_or_default();
        message["event"] = "rtx_hdr_gpu_timing".into();
        message["scope"] = "gl_sampling_and_cuda_pipeline_not_end_to_end".into();
        let _ = logger.emit(
            amaterasu_module_sdk::LogLevel::Info,
            "rtx-hdr",
            &message.to_string(),
        );
    }

    fn retire_inputs(&mut self) {
        // Teardown is the sole synchronous barrier. Ordinary frame dispatch
        // only polls and returns BUSY when bounded storage is occupied.
        if unsafe { am_rtx_finish(self.session.0.as_ptr()) } != 0 {
            std::process::abort();
        }
        self.pending_inputs.clear();
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
    /// Negotiated resource outputs per configured stream.
    resource_outputs: BTreeMap<u64, resource::ResourceOutput>,
    device_uuid: Option<[u8; 16]>,
    input_offers: input::InputOffers,
    selected_inputs: BTreeMap<u64, input::SelectedInput>,
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
            resource_outputs: BTreeMap::new(),
            device_uuid: None,
            input_offers: input::InputOffers::new(),
            selected_inputs: BTreeMap::new(),
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

    /// Opens the stream on its first frame and applies the current
    /// enablement. Returns whether conversion is active for this frame.
    fn prepare(&mut self, stream: u64, frame: &CaptureResourceFrame) -> Result<bool, Status> {
        if self.retired.contains(&stream) {
            return Err(MEDIA_CLOSED);
        }
        let enabled = self.conversion_enabled(stream)?;
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
        let active = self.streams.get_mut(&stream).expect("stream was opened");
        if active.enabled != enabled {
            active.configure(settings, enabled, self.logger.as_ref())?;
            active.enabled = enabled;
        }
        Ok(enabled)
    }

    fn conversion_enabled(&self, stream: u64) -> Result<bool, Status> {
        let settings = self.stream_settings.get(&stream).unwrap_or(&self.settings);
        if !settings.enabled {
            return Ok(false);
        }
        if !settings.foreground_only && !settings.fullscreen_only {
            return Ok(true);
        }
        let Some(policy) = &self.policy else {
            return Ok(false);
        };
        let policy = match policy.try_lock() {
            Ok(policy) => policy,
            Err(std::sync::TryLockError::WouldBlock) => return Err(MEDIA_BUSY),
            Err(_) => return Err(MEDIA_BACKEND_ERROR),
        };
        Ok(policy.enabled(
            self.targets.get(&stream).and_then(Option::as_ref),
            settings.foreground_only,
            settings.fullscreen_only,
            std::time::Instant::now(),
        ))
    }

    fn report_conversion(&mut self, stream: u64, frame: &CaptureResourceFrame, enabled: bool) {
        let settings = self.stream_settings.get(&stream).unwrap_or(&self.settings);
        let Some(active) = self.streams.get_mut(&stream) else {
            return;
        };
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
    }

    /// The selected device's UUID, which names the CUDA context frames live in.
    fn device_uuid(&mut self) -> Result<[u8; 16], Status> {
        if let Some(uuid) = self.device_uuid {
            return Ok(uuid);
        }
        let uuid = resource::device_uuid(&self.settings, self.logger.as_ref())?;
        self.device_uuid = Some(uuid);
        Ok(uuid)
    }
}
impl Drop for Processor {
    fn drop(&mut self) {
        self.clear_demand();
    }
}
impl FrameProcessor for Processor {
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
    fn input_offers(&mut self, _: u64) -> Result<(u64, Vec<CaptureResourceOffer>), Status> {
        Ok(self.input_offers.enumerate())
    }
    fn configure_input(
        &mut self,
        stream: u64,
        generation: u64,
        offer: u64,
        _: u64,
        source: &CaptureResourceOffer,
    ) -> Result<(), Status> {
        let selected = self.input_offers.select(generation, offer, source)?;
        self.selected_inputs.insert(stream, selected);
        Ok(())
    }
    fn output_offers(&mut self, stream: u64) -> Result<(u64, Vec<CaptureResourceOffer>), Status> {
        if self.retired.contains(&stream) {
            return Err(MEDIA_CLOSED);
        }
        if !self.stream_settings.contains_key(&stream) && !self.streams.contains_key(&stream) {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let uuid = self.device_uuid()?;
        let offers = self
            .resource_outputs
            .entry(stream)
            .or_insert_with(|| resource::ResourceOutput::new(uuid));
        Ok((offers.generation(), vec![offers.offer()]))
    }
    fn select_output(&mut self, stream: u64, generation: u64, offer: u64) -> Result<(), Status> {
        if self.retired.contains(&stream) {
            return Err(MEDIA_CLOSED);
        }
        self.resource_outputs
            .get_mut(&stream)
            .ok_or(MEDIA_UNSUPPORTED)?
            .select(generation, offer)
    }
    fn process(
        &mut self,
        stream: u64,
        source: ProcessingFrame<'_>,
    ) -> Result<ResourceOutputFrame, Status> {
        let input = self
            .selected_inputs
            .get(&stream)
            .ok_or(MEDIA_UNSUPPORTED)?
            .frame(source)?;
        let handoff = self
            .resource_outputs
            .get(&stream)
            .and_then(resource::ResourceOutput::selection)
            .ok_or(MEDIA_UNSUPPORTED)?;
        let device_uuid = self.device_uuid()?;
        let output_buffers = self
            .stream_settings
            .get(&stream)
            .unwrap_or(&self.settings)
            .output_buffers;
        resource::validate_storage_budget(
            input.descriptor().width,
            input.descriptor().height,
            output_buffers,
        )?;
        let enabled = self.prepare(stream, input.descriptor())?;
        let active = self.streams.get_mut(&stream).expect("stream was prepared");
        let output = active.process_resource(
            &input,
            handoff,
            output_buffers,
            device_uuid,
            self.logger.as_ref(),
        )?;
        self.report_conversion(stream, input.descriptor(), enabled);
        Ok(output)
    }
    fn cancel(&mut self, stream: u64) {
        self.set_demand(stream, false);
        self.stream_settings.remove(&stream);
        self.targets.remove(&stream);
        self.resource_outputs.remove(&stream);
        self.selected_inputs.remove(&stream);
        if let Some(mut active) = self.streams.remove(&stream) {
            active.cached_resource = None;
            active.retire_inputs();
        }
        if self.retired.len() < 1024 {
            self.retired.insert(stream);
        }
    }
    fn shutdown(&mut self) {
        self.resource_outputs.clear();
        self.selected_inputs.clear();
        for stream in self.streams.values_mut() {
            stream.cached_resource = None;
            stream.retire_inputs();
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

mod input;
mod resource;
#[cfg(test)]
mod tests;

pub(crate) fn retirement_ready() -> Result<bool, Status> {
    resource::retirement_ready()
        .map(|retired| retired && NATIVE_SESSIONS.load(Ordering::Acquire) == 0)
}
