//! CUDA output for the canonical frame processor. The converted frame stays in a
//! CUDA slot of the device's primary context, ordered for the consumer by the
//! slot's event, so neither a GL output draw nor a sync_file handoff follows
//! the model. A slot is rewritten only once every frame reference is released.
use super::{
    NativeSession, PendingInput, ReadCompletion, Stream, backend_failure,
    input::{InputFrame, Storage},
};
use crate::Settings;
use amaterasu_media::*;
use amaterasu_module_sdk::{
    abi::*,
    frame_processing::{FrameResourceSelection, ResourceOutputFrame},
};
use std::os::unix::ffi::OsStrExt;
use std::{
    cell::Cell,
    ffi::{CString, c_char, c_void},
    os::fd::{FromRawFd, OwnedFd},
    ptr::NonNull,
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

const MAX_OUTPUT_REFERENCES: usize = 64;
const MAX_CUDA_BYTES: usize = 256 * 1024 * 1024;
static OUTPUT_REFERENCES: AtomicUsize = AtomicUsize::new(0);
static CUDA_BYTES: AtomicUsize = AtomicUsize::new(0);
static QUARANTINED: AtomicBool = AtomicBool::new(false);

pub(super) fn validate_storage_budget(width: u32, height: u32, count: usize) -> Result<(), Status> {
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| bytes.checked_mul(count));
    if count == 0 || !bytes.is_some_and(|bytes| bytes <= MAX_CUDA_BYTES) {
        return Err(MEDIA_UNSUPPORTED);
    }
    Ok(())
}
pub(super) fn retirement_ready() -> Result<bool, Status> {
    if QUARANTINED.load(Ordering::Acquire) {
        return Err(MEDIA_BACKEND_ERROR);
    }
    Ok(OUTPUT_REFERENCES.load(Ordering::Acquire) == 0 && CUDA_BYTES.load(Ordering::Acquire) == 0)
}
pub(super) fn reserve(counter: &AtomicUsize, count: usize, limit: usize) -> Result<(), Status> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current
            .checked_add(count)
            .filter(|next| *next <= limit)
            .ok_or(MEDIA_BUSY)?;
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

/// Reserve output admission before queuing any GPU work. Every failure path
/// returns capacity; a delivered reference transfers it to release_slot.
struct OutputAdmission(bool);
impl OutputAdmission {
    fn new() -> Result<Self, Status> {
        reserve(&OUTPUT_REFERENCES, 1, MAX_OUTPUT_REFERENCES)?;
        Ok(Self(true))
    }
    fn commit(mut self) {
        self.0 = false;
    }
}
impl Drop for OutputAdmission {
    fn drop(&mut self) {
        if self.0 {
            OUTPUT_REFERENCES.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

unsafe extern "C" {
    fn am_rtx_device_uuid(
        root: *const c_char,
        device: i32,
        uuid: *mut u8,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn am_rtx_cuda_output_open(
        context: *mut c_void,
        pixels: *mut u64,
        pitch: *mut u32,
        ready: *mut *mut c_void,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn am_rtx_cuda_output_close(context: *mut c_void, output: *mut c_void);
    fn am_rtx_process_cpu_cuda(
        context: *mut c_void,
        frame: *const CpuFrame,
        color: *const FrameColorMetadata,
        output: *mut c_void,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn am_rtx_cuda_output_ready(context: *mut c_void, output: *mut c_void) -> i32;
    fn am_rtx_process_cuda(
        context: *mut c_void,
        frame: *const DmaBufFrame,
        output: *mut c_void,
        input_fence: *mut i32,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
}

/// The single output offer: a stream's offers never change once enumerated.
const GENERATION: u64 = 1;
const OFFER: u64 = 1;

pub(super) fn device_uuid(
    settings: &Settings,
    logger: Option<&amaterasu_module_sdk::LogEmitter>,
) -> Result<[u8; 16], Status> {
    let root = CString::new(settings.runtime_directory.as_os_str().as_bytes())
        .map_err(|_| STATUS_INVALID_ARGUMENT)?;
    let mut uuid = [0u8; 16];
    let mut error = [0; 512];
    // SAFETY: the path, UUID and error buffers are live for the call.
    let result = unsafe {
        am_rtx_device_uuid(
            root.as_ptr(),
            settings.device as i32,
            uuid.as_mut_ptr(),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if result != 0 {
        // Without a device identity no consumer can verify this handoff.
        // Reject negotiation instead of substituting another output route.
        let _ = backend_failure(&error, logger);
        return Err(MEDIA_UNSUPPORTED);
    }
    Ok(uuid)
}

/// A stream's CUDA output offer, kept at a stable address while enumerated.
pub(super) struct ResourceOutput {
    payload: Box<(CudaDeviceCapabilities, CudaEventCapabilities)>,
    selected: bool,
}

const NONE: FrameDescriptor = FrameDescriptor {
    kind: FRAME_SYNC_NONE,
    version: 0,
    size: 0,
    reserved: 0,
    namespace: [0; 16],
    data: std::ptr::null(),
};

impl ResourceOutput {
    pub(super) fn new(device_uuid: [u8; 16]) -> Self {
        Self {
            payload: Box::new((
                CudaDeviceCapabilities {
                    device_uuid,
                    format: CUDA_FORMAT_X2B10G10R10,
                    min_width: 16,
                    max_width: 8192,
                    min_height: 16,
                    max_height: 8192,
                    reserved: 0,
                },
                CudaEventCapabilities { device_uuid },
            )),
            selected: false,
        }
    }

    pub(super) fn generation(&self) -> u64 {
        GENERATION
    }

    /// The offer's descriptors point into this record, which the processor
    /// keeps until the stream is cancelled.
    pub(super) fn offer(&self) -> CaptureResourceOffer {
        CaptureResourceOffer {
            id: OFFER,
            ownership: FRAME_OWNERSHIP_RETAINABLE,
            accessibility: FRAME_ACCESS_GPU,
            reserved: 0,
            resource: cuda_descriptor(FRAME_RESOURCE_CUDA_DEVICE, &self.payload.0),
            acquire_sync: cuda_descriptor(FRAME_SYNC_CUDA_EVENT, &self.payload.1),
            completion_sync: NONE,
        }
    }

    pub(super) fn select(&mut self, generation: u64, offer: u64) -> Result<(), Status> {
        if generation != GENERATION || offer != OFFER {
            return Err(MEDIA_UNSUPPORTED);
        }
        self.selected = true;
        Ok(())
    }

    pub(super) fn selection(&self) -> Option<FrameResourceSelection> {
        self.selected.then_some(FrameResourceSelection {
            generation: GENERATION,
            offer: OFFER,
        })
    }
}

/// One CUDA output buffer and its ready event. Frame payloads live here so
/// every delivered descriptor stays valid while the slot is referenced.
pub(super) struct CudaSlot {
    session: Rc<NativeSession>,
    output: NonNull<c_void>,
    frame: CudaDeviceFrame,
    ready: CudaEventSync,
    references: Cell<u32>,
    input_references: Cell<u32>,
    /// A consumer released with an unknown completion object: never reuse.
    quarantined: Cell<bool>,
    allocated_bytes: usize,
}

impl CudaSlot {
    fn open(
        session: &Rc<NativeSession>,
        device_uuid: [u8; 16],
        width: u32,
        height: u32,
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<Self, Status> {
        let mut pixels = 0u64;
        let mut pitch = 0u32;
        let mut ready = std::ptr::null_mut();
        let mut error = [0; 512];
        // SAFETY: the session is live and the out-pointers are writable.
        let raw = unsafe {
            am_rtx_cuda_output_open(
                session.0.as_ptr(),
                &mut pixels,
                &mut pitch,
                &mut ready,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let output = NonNull::new(raw).ok_or_else(|| backend_failure(&error, logger))?;
        let Some(allocated_bytes) = (pitch as usize).checked_mul(height as usize) else {
            // SAFETY: the newly allocated native slot owns no queued reads.
            unsafe { am_rtx_cuda_output_close(session.0.as_ptr(), output.as_ptr()) };
            return Err(MEDIA_BACKEND_ERROR);
        };
        if let Err(status) = reserve(&CUDA_BYTES, allocated_bytes, MAX_CUDA_BYTES) {
            // SAFETY: this newly allocated slot has never been exposed or used.
            unsafe { am_rtx_cuda_output_close(session.0.as_ptr(), output.as_ptr()) };
            return Err(status);
        }
        Ok(Self {
            session: session.clone(),
            output,
            frame: CudaDeviceFrame {
                device_uuid,
                device_pointer: pixels,
                pitch,
                format: CUDA_FORMAT_X2B10G10R10,
                width,
                height,
            },
            ready: CudaEventSync { event: ready },
            references: Cell::new(0),
            input_references: Cell::new(0),
            quarantined: Cell::new(false),
            allocated_bytes,
        })
    }

    pub(super) fn is_referenced(&self) -> bool {
        self.references.get() != 0 || self.input_references.get() != 0 || self.quarantined.get()
    }

    /// Adds one reference and describes it as a frame of the selected offer.
    pub(super) fn input_completed(&self) -> bool {
        // SAFETY: the slot and session stay live; this only queries the event.
        unsafe { am_rtx_cuda_output_ready(self.session.0.as_ptr(), self.output.as_ptr()) == 1 }
    }
    pub(super) fn release_input(&self) {
        self.input_references.set(self.input_references.get() - 1);
    }
    pub(super) fn quarantine(self: &Rc<Self>) {
        if !self.quarantined.replace(true) {
            QUARANTINED.store(true, Ordering::Release);
            // SAFETY: retain the native allocation and session indefinitely;
            // an unproven read must prevent both reuse and library retirement.
            unsafe { Rc::increment_strong_count(Rc::as_ptr(self)) };
        }
    }
    fn deliver(
        self: &Rc<Self>,
        handoff: FrameResourceSelection,
        input: &CaptureResourceFrame,
        freshness: u32,
    ) -> Result<ResourceOutputFrame, Status> {
        if self.references.get() >= 64 {
            return Err(MEDIA_BUSY);
        }
        self.deliver_admitted(handoff, input, freshness, OutputAdmission::new()?)
    }
    fn deliver_admitted(
        self: &Rc<Self>,
        handoff: FrameResourceSelection,
        input: &CaptureResourceFrame,
        freshness: u32,
        admission: OutputAdmission,
    ) -> Result<ResourceOutputFrame, Status> {
        self.references.set(self.references.get() + 1);
        // SAFETY: the initial output reference owns this strong count, so its
        // descriptors remain valid after the processing stream is cancelled.
        unsafe { Rc::increment_strong_count(Rc::as_ptr(self)) };
        let frame = CaptureResourceFrame {
            offer_generation: handoff.generation,
            offer_id: handoff.offer,
            timestamp_us: input.timestamp_us,
            capture_pacing_timestamp_us: input.capture_pacing_timestamp_us,
            sequence: input.sequence,
            width: self.frame.width,
            height: self.frame.height,
            freshness,
            reserved: 0,
            color: FrameColorMetadata {
                version: 1,
                size: std::mem::size_of::<FrameColorMetadata>() as u32,
                primaries: FRAME_PRIMARIES_BT2020,
                transfer: FRAME_TRANSFER_PQ,
                matrix: FRAME_MATRIX_RGB,
                range: FRAME_RANGE_FULL,
            },
            clock: input.clock,
            pacing_clock: input.pacing_clock,
            resource: cuda_descriptor(FRAME_RESOURCE_CUDA_DEVICE, &self.frame),
            acquire_sync: cuda_descriptor(FRAME_SYNC_CUDA_EVENT, &self.ready),
            context: Rc::as_ptr(self).cast_mut().cast(),
            retain: retain_slot,
            release: release_slot,
        };
        // SAFETY: the reference added above transfers to the output frame;
        // the stream keeps this slot alive while any reference remains.
        admission.commit();
        Ok(unsafe { ResourceOutputFrame::from_owned(frame) })
    }
}

impl Drop for CudaSlot {
    fn drop(&mut self) {
        assert!(
            !self.is_referenced(),
            "CUDA output destroyed with live references"
        );
        // SAFETY: this owns one output of a live session; close drains its writes.
        unsafe { am_rtx_cuda_output_close(self.session.0.as_ptr(), self.output.as_ptr()) };
        CUDA_BYTES.fetch_sub(self.allocated_bytes, Ordering::AcqRel);
    }
}

unsafe extern "C" fn retain_slot(context: *mut c_void) -> Status {
    // SAFETY: a live frame reference keeps its slot alive in the stream.
    let slot = unsafe { &*context.cast::<CudaSlot>() };
    if slot.references.get() >= 64 {
        return MEDIA_BUSY;
    }
    if let Err(status) = reserve(&OUTPUT_REFERENCES, 1, MAX_OUTPUT_REFERENCES) {
        return status;
    }
    slot.references.set(slot.references.get() + 1);
    // SAFETY: the live borrowed/retained reference pins this exact Rc owner.
    unsafe { Rc::increment_strong_count(context.cast::<CudaSlot>()) };
    STATUS_OK
}

unsafe extern "C" fn release_slot(context: *mut c_void, completion: *const FrameDescriptor) {
    // SAFETY: the released reference kept this slot alive until now.
    let slot = unsafe { Rc::from_raw(context.cast::<CudaSlot>()) };
    // The offer negotiated NONE: anything else cannot prove the reads ended,
    // so the slot is never written again.
    // SAFETY: a non-null completion is borrowed for this call.
    let valid = !completion.is_null() && {
        let d = unsafe { &*completion };
        d.kind == FRAME_SYNC_NONE
            && d.version == 0
            && d.size == 0
            && d.reserved == 0
            && d.namespace == [0; 16]
            && d.data.is_null()
    };
    if !valid {
        slot.quarantine();
    }
    let Some(remaining) = slot.references.get().checked_sub(1) else {
        std::process::abort();
    };
    slot.references.set(remaining);
    drop(slot);
    OUTPUT_REFERENCES.fetch_sub(1, Ordering::AcqRel);
}

/// The latest output, kept referenced for cached-frame repeats.
pub(super) struct CachedResource {
    slot: Rc<CudaSlot>,
    input_sequence: u64,
}

impl Drop for CachedResource {
    fn drop(&mut self) {
        // This internal reference is owned by the cache's Rc, independent of
        // ABI reader references. Dropping a stream cannot consume another reader.
        self.slot.references.set(
            self.slot
                .references
                .get()
                .checked_sub(1)
                .expect("cached CUDA reference"),
        );
    }
}

impl Stream {
    pub(super) fn process_resource(
        &mut self,
        source: &InputFrame<'_>,
        handoff: FrameResourceSelection,
        output_buffers: usize,
        device_uuid: [u8; 16],
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<ResourceOutputFrame, Status> {
        let input = source.descriptor();
        if input.width != self.width || input.height != self.height {
            return Err(MEDIA_UNSUPPORTED);
        }
        self.retire_completed_inputs();
        self.report_timing(logger);
        if source.freshness() == CAPTURE_FRAME_CACHED_REUSE
            && let Some(cached) = self.cached_resource.as_ref()
            && cached.input_sequence == input.sequence
        {
            return cached.slot.deliver(handoff, input, source.freshness());
        }
        if self.pending_inputs.len() >= super::MAX_PENDING_INPUTS {
            return Err(MEDIA_BUSY);
        }
        let admission = OutputAdmission::new()?;
        self.cached_resource = None;
        self.open_cuda_slots(output_buffers, device_uuid, logger)?;
        let slot = self
            .cuda_slots
            .iter()
            .find(|slot| !slot.is_referenced())
            .ok_or(MEDIA_BUSY)?
            .clone();
        let retained = source.retain()?;
        let mut error = [0; 512];
        let mut fence = -1;
        // SAFETY: the SDK lends the input until this returns, and `retained`
        // extends that until the returned fence signals. The idle slot has no
        // reference, so nothing reads it while it is rewritten.
        let result = match &source.storage {
            Storage::DmaBuf(frame) => unsafe {
                am_rtx_process_cuda(
                    slot.session.0.as_ptr(),
                    frame,
                    slot.output.as_ptr(),
                    &mut fence,
                    error.as_mut_ptr(),
                    error.len(),
                )
            },
            Storage::Cpu(frame) => unsafe {
                am_rtx_process_cpu_cuda(
                    slot.session.0.as_ptr(),
                    frame,
                    &input.color,
                    slot.output.as_ptr(),
                    error.as_mut_ptr(),
                    error.len(),
                )
            },
        };
        if result != 0 {
            // SAFETY: failed native calls drain any reads before returning.
            unsafe { retained.release_completed() };
            return Err(backend_failure(&error, logger));
        }
        let completion = match &source.storage {
            Storage::DmaBuf(_) if fence >= 0 => {
                ReadCompletion::Fence(unsafe { OwnedFd::from_raw_fd(fence) })
            }
            Storage::DmaBuf(_) => {
                // A successful unfenced DMA-BUF read is not this contract.
                retained.quarantine();
                slot.quarantine();
                return Err(MEDIA_BACKEND_ERROR);
            }
            Storage::Cpu(_) => {
                slot.input_references.set(slot.input_references.get() + 1);
                ReadCompletion::Cuda(slot.clone())
            }
        };
        self.queue_pending_input(Some(PendingInput {
            input: Some(retained),
            completion,
        }));
        self.cached_resource = Some(CachedResource {
            slot: slot.clone(),
            input_sequence: input.sequence,
        });
        slot.references.set(slot.references.get() + 1);
        slot.deliver_admitted(handoff, input, source.freshness(), admission)
    }

    fn open_cuda_slots(
        &mut self,
        count: usize,
        device_uuid: [u8; 16],
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<(), Status> {
        if !self.cuda_slots.is_empty() {
            return Ok(());
        }
        let session = self.session.clone();
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            slots.push(Rc::new(CudaSlot::open(
                &session,
                device_uuid,
                self.width,
                self.height,
                logger,
            )?));
        }
        self.cuda_slots = slots;
        Ok(())
    }
}
