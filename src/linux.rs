//! The only unsafe code here wraps the private GPU backend and lease ABI.
//! Every input read and output write completes before returning to the SDK.
use crate::Settings;
use amaterasu_media::*;
use amaterasu_module_sdk::{
    abi::*,
    processing::{GpuFrame, GpuFrameProcessor, GpuOutputFrame},
};
use std::os::unix::ffi::OsStrExt;
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString, c_char, c_void},
    ptr::NonNull,
    rc::Rc,
};

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
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn am_rtx_wait_fence(fd: i32) -> i32;
}

struct NativeSession(NonNull<c_void>);
impl Drop for NativeSession {
    fn drop(&mut self) {
        // SAFETY: all output surfaces hold an Rc to this session; GPU work is
        // synchronous, so the final reference certifies storage is no longer read.
        unsafe { am_rtx_close(self.0.as_ptr()) };
    }
}
struct Slot {
    session: Rc<NativeSession>,
    output: NonNull<c_void>,
    frame: DmaBufFrame,
    busy: Cell<bool>,
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
    width: u32,
    height: u32,
    enabled: bool,
    reported_enabled: Option<bool>,
}
struct Lease {
    slot: Rc<Slot>,
    references: Cell<u32>,
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
            }));
        }
        Ok(Self {
            slots,
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
        &self,
        settings: &Settings,
        enabled: bool,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<(), Status> {
        Self::configure_session(&self.slots[0].session, settings, enabled, logger)
    }

    fn process(
        &self,
        input: &DmaBufFrame,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<GpuOutputFrame, Status> {
        if input.width != self.width || input.height != self.height {
            return Err(MEDIA_UNSUPPORTED);
        }
        let slot = self
            .slots
            .iter()
            .find(|slot| !slot.busy.get())
            .ok_or(MEDIA_BUSY)?;
        let mut error = [0; 512];
        // SAFETY: the SDK lends immutable input pixels until this returns. This
        // idle slot is exclusively writable, with no downstream reference.
        let result = unsafe {
            am_rtx_process(
                slot.session.0.as_ptr(),
                input,
                slot.output.as_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result != 0 {
            return Err(backend_failure(&error, logger));
        }
        slot.busy.set(true);
        let frame = DmaBufFrame {
            sequence: input.sequence,
            timestamp_us: input.timestamp_us,
            ..slot.frame
        };
        let lease = Box::new(Lease {
            slot: slot.clone(),
            references: Cell::new(1),
        });
        // SAFETY: transfers the initial reference. The box keeps the immutable
        // slot/session alive; retain/release are bounded and own completion fds.
        unsafe {
            GpuOutputFrame::from_owned_lease(DmaBufFrameLease {
                frame,
                context: Box::into_raw(lease).cast(),
                retain: Some(retain),
                release: Some(release),
            })
        }
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
        let output = active.process(frame, self.logger.as_ref())?;
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
        if let Some(active) = self.streams.remove(&stream) {
            active.assert_retired();
        }
        if self.retired.len() < 1024 {
            self.retired.insert(stream);
        }
    }
    fn shutdown(&mut self) {
        for stream in self.streams.values() {
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
