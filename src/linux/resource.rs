//! CUDA resource output (GPU processor v1.4). The converted frame stays in a
//! CUDA slot of the device's primary context, ordered for the consumer by the
//! slot's event, so neither a GL output draw nor a sync_file handoff follows
//! the model. A slot is rewritten only once every frame reference is released.
use super::{NativeSession, PendingInput, Stream, am_rtx_wait_fence, backend_failure};
use crate::Settings;
use amaterasu_media::*;
use amaterasu_module_sdk::{
    abi::*,
    processing::{GpuFrame, ResourceHandoff, ResourceOutputFrame, RetainedGpuInput},
};
use std::os::unix::ffi::OsStrExt;
use std::{
    cell::Cell,
    ffi::{CString, c_char, c_void},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    rc::Rc,
};

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
        // Without a device identity no consumer can verify the handoff;
        // report it and keep the lease path.
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

    pub(super) fn selection(&self) -> Option<ResourceHandoff> {
        self.selected.then_some(ResourceHandoff {
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
    /// A consumer released with an unknown completion object: never reuse.
    quarantined: Cell<bool>,
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
            quarantined: Cell::new(false),
        })
    }

    pub(super) fn is_referenced(&self) -> bool {
        self.references.get() != 0 || self.quarantined.get()
    }

    /// Adds one reference and describes it as a frame of the selected offer.
    fn deliver(
        self: &Rc<Self>,
        handoff: ResourceHandoff,
        input: &DmaBufFrame,
        freshness: u32,
    ) -> ResourceOutputFrame {
        self.references.set(self.references.get() + 1);
        let frame = CaptureResourceFrame {
            offer_generation: handoff.generation,
            offer_id: handoff.offer,
            timestamp_us: input.timestamp_us,
            capture_pacing_timestamp_us: 0,
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
            clock: FrameClockMetadata::default(),
            pacing_clock: FrameClockMetadata::default(),
            resource: cuda_descriptor(FRAME_RESOURCE_CUDA_DEVICE, &self.frame),
            acquire_sync: cuda_descriptor(FRAME_SYNC_CUDA_EVENT, &self.ready),
            context: Rc::as_ptr(self).cast_mut().cast(),
            retain: retain_slot,
            release: release_slot,
        };
        // SAFETY: the reference added above transfers to the output frame;
        // the stream keeps this slot alive while any reference remains.
        unsafe { ResourceOutputFrame::from_owned(frame) }
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
    }
}

unsafe extern "C" fn retain_slot(context: *mut c_void) -> Status {
    // SAFETY: a live frame reference keeps its slot alive in the stream.
    let slot = unsafe { &*context.cast::<CudaSlot>() };
    if slot.references.get() >= 64 {
        return MEDIA_BUSY;
    }
    slot.references.set(slot.references.get() + 1);
    STATUS_OK
}

unsafe extern "C" fn release_slot(context: *mut c_void, completion: *const FrameDescriptor) {
    // SAFETY: the released reference kept this slot alive until now.
    let slot = unsafe { &*context.cast::<CudaSlot>() };
    // The offer negotiated NONE: anything else cannot prove the reads ended,
    // so the slot is never written again.
    // SAFETY: a non-null completion is borrowed for this call.
    if completion.is_null() || unsafe { (*completion).kind } != FRAME_SYNC_NONE {
        slot.quarantined.set(true);
    }
    let Some(remaining) = slot.references.get().checked_sub(1) else {
        std::process::abort();
    };
    slot.references.set(remaining);
}

/// The latest output, kept referenced for cached-frame repeats.
pub(super) struct CachedResource {
    slot: Rc<CudaSlot>,
    input_sequence: u64,
}

impl Drop for CachedResource {
    fn drop(&mut self) {
        // SAFETY: the cache holds one reference added when it was created.
        unsafe { release_slot(Rc::as_ptr(&self.slot).cast_mut().cast(), &NONE) };
    }
}

impl Stream {
    pub(super) fn process_resource(
        &mut self,
        source: &GpuFrame<'_>,
        handoff: ResourceHandoff,
        output_buffers: usize,
        device_uuid: [u8; 16],
        logger: Option<&amaterasu_module_sdk::LogEmitter>,
    ) -> Result<ResourceOutputFrame, Status> {
        let input = source.frame();
        if input.width != self.width || input.height != self.height {
            return Err(MEDIA_UNSUPPORTED);
        }
        self.retire_completed_inputs();
        self.report_timing(logger);
        if source.freshness() == CAPTURE_FRAME_CACHED_REUSE
            && let Some(cached) = self.cached_resource.as_ref()
            && cached.input_sequence == input.sequence
        {
            return Ok(cached.slot.deliver(handoff, input, source.freshness()));
        }
        self.cached_resource = None;
        self.open_cuda_slots(output_buffers, device_uuid, logger)?;
        let slot = self
            .cuda_slots
            .iter()
            .find(|slot| !slot.is_referenced())
            .ok_or(MEDIA_BUSY)?
            .clone();
        let retained = source.retain().ok();
        let mut error = [0; 512];
        let mut fence = -1;
        // SAFETY: the SDK lends the input until this returns, and `retained`
        // extends that until the returned fence signals. The idle slot has no
        // reference, so nothing reads it while it is rewritten.
        let result = unsafe {
            am_rtx_process_cuda(
                slot.session.0.as_ptr(),
                input,
                slot.output.as_ptr(),
                &mut fence,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result != 0 {
            if let Some(retained) = retained {
                // SAFETY: the failed call synchronized every queued read.
                unsafe { retained.release_completed() };
            }
            return Err(backend_failure(&error, logger));
        }
        let pending = retain_until_read(fence, retained);
        self.queue_pending_input(pending);
        self.cached_resource = Some(CachedResource {
            slot: slot.clone(),
            input_sequence: input.sequence,
        });
        slot.references.set(slot.references.get() + 1);
        Ok(slot.deliver(handoff, input, source.freshness()))
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
        let session = self.slots[0].session.clone();
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count.max(2) {
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

/// Keeps a retained input until `fence` (covering its only read) signals.
/// Without a fence the read completed; without a reference, wait for it here.
fn retain_until_read(fence: i32, retained: Option<RetainedGpuInput>) -> Option<PendingInput> {
    if fence < 0 {
        if let Some(retained) = retained {
            // SAFETY: no fence means the sampling read completed synchronously.
            unsafe { retained.release_completed() };
        }
        return None;
    }
    // SAFETY: the backend transfers ownership of this new sync_file.
    let fence = unsafe { OwnedFd::from_raw_fd(fence) };
    match retained {
        Some(retained) => Some(PendingInput {
            input: Some(retained),
            completion: Some(fence),
        }),
        None => {
            // SAFETY: the borrowed descriptor stays open throughout the wait.
            if unsafe { am_rtx_wait_fence(fence.as_raw_fd()) } != 0 {
                std::process::abort();
            }
            None
        }
    }
}
