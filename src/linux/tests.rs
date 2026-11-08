use super::*;
use amaterasu_module_sdk::frame_processing::{
    configure_frame_stream, into_frame_processor_binding, update_frame_stream,
};
use std::cell::Cell;
fn settings() -> Settings {
    serde_json::from_value(serde_json::json!({"runtime_directory":"/runtime"})).unwrap()
}
fn frame() -> DmaBufFrame {
    let mut planes = [DmaBufPlane::UNUSED; DMABUF_MAX_PLANES];
    planes[0] = DmaBufPlane {
        fd: 0,
        acquire_fence_fd: -1,
        offset: 0,
        pitch: 1024,
    };
    DmaBufFrame {
        width: 256,
        height: 144,
        fourcc: u32::from_le_bytes(*b"XR24"),
        plane_count: 1,
        modifier: 0,
        planes,
        color: DMABUF_COLOR_SRGB,
        reserved: 0,
        sequence: 1,
        timestamp_us: 66,
    }
}

fn intent_settings() -> (serde_json::Value, serde_json::Value) {
    let mut settings = serde_json::to_value(
        amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
            .unwrap(),
    )
    .unwrap();
    settings["rtx_hdr_enable"] = true.into();
    settings["rtx_hdr_scope"] = "global".into();
    settings["rtx_hdr_peak_brightness"] = 1000.into();
    let target = serde_json::json!({"kind":"stream","host_epoch":"epoch","session_id":"session","stream_id":"stream","generation":1});
    let request = serde_json::json!({"method":"processing_intent","interface_major":1,"interface_minor":1,"arguments":{"target":target,"revision":"snapshot","settings":settings}});
    let response = crate::lifecycle::intent("consumer", &["consumer".into()], request);
    assert_eq!(response["status"], "succeeded");
    (response["processor_settings"].clone(), settings)
}

#[test]
fn foreground_restrictions_are_rejected_without_an_observer() {
    let (mut initial, mut update) = intent_settings();
    let mut processor = Processor::new(settings());
    initial["rtx_hdr_foreground_only"] = true.into();
    assert_eq!(
        processor.configure_stream(1, &serde_json::to_vec(&initial).unwrap()),
        Err(STATUS_INVALID_ARGUMENT)
    );
    initial["rtx_hdr_foreground_only"] = false.into();
    processor
        .configure_stream(1, &serde_json::to_vec(&initial).unwrap())
        .unwrap();
    update["rtx_hdr_fullscreen_only"] = true.into();
    assert_eq!(
        processor.update_stream(1, &serde_json::to_vec(&update).unwrap()),
        Err(STATUS_INVALID_ARGUMENT)
    );
}

#[test]
fn intent_target_survives_registry_updates_and_demand_is_retired() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let policy = Arc::new(Mutex::new(
        crate::foreground_policy::ForegroundPolicy::default(),
    ));
    let (initial, mut registry) = intent_settings();
    let mut processor = Processor::new(settings()).with_policy(policy, counter.clone());
    processor
        .configure_stream(9, &serde_json::to_vec(&initial).unwrap())
        .unwrap();
    let target = processor.targets[&9].clone();
    registry["rtx_hdr_foreground_only"] = true.into();
    registry["rtx_hdr_peak_brightness"] = 1800.into();
    processor
        .update_stream(9, &serde_json::to_vec(&registry).unwrap())
        .unwrap();
    assert_eq!(processor.targets[&9], target);
    assert_eq!(processor.stream_settings[&9].peak_luminance, 1800);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    let mut changed = registry.clone();
    changed["foreground_target"] = serde_json::Value::Null;
    assert_eq!(
        processor.update_stream(9, &serde_json::to_vec(&changed).unwrap()),
        Err(STATUS_INVALID_ARGUMENT)
    );
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    registry["rtx_hdr_foreground_only"] = false.into();
    processor
        .update_stream(9, &serde_json::to_vec(&registry).unwrap())
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    registry["rtx_hdr_fullscreen_only"] = true.into();
    processor
        .update_stream(9, &serde_json::to_vec(&registry).unwrap())
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    registry["rtx_hdr_foreground_only"] = true.into();
    processor
        .update_stream(9, &serde_json::to_vec(&registry).unwrap())
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    registry["rtx_hdr_fullscreen_only"] = false.into();
    processor
        .update_stream(9, &serde_json::to_vec(&registry).unwrap())
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    processor.cancel(9);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    processor
        .configure_stream(10, &serde_json::to_vec(&initial).unwrap())
        .unwrap();
    processor
        .update_stream(10, &serde_json::to_vec(&registry).unwrap())
        .unwrap();
    drop(processor);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

#[test]
fn foreground_demand_counts_each_stream_and_clears_on_shutdown() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let policy = Arc::new(Mutex::new(
        crate::foreground_policy::ForegroundPolicy::default(),
    ));
    let (mut initial, _) = intent_settings();
    initial["rtx_hdr_foreground_only"] = true.into();
    let mut startup = settings();
    startup.max_streams = 2;
    let mut processor = Processor::new(startup).with_policy(policy, counter.clone());
    let payload = serde_json::to_vec(&initial).unwrap();
    processor.configure_stream(1, &payload).unwrap();
    processor.configure_stream(2, &payload).unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 2);
    processor.cancel(1);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    processor.cancel(1);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    processor.shutdown();
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    drop(processor);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

#[test]
fn stream_controls_update_atomically_and_retire_with_stream() {
    let mut processor = Processor::new(settings());
    let json = br#"{"rtx_hdr_enable":true,"rtx_hdr_peak_brightness":1400,"rtx_hdr_middle_gray":50,"rtx_hdr_contrast":5,"rtx_hdr_saturation":-5,"rtx_hdr_debanding":true}"#;
    processor.configure_stream(9, json).unwrap();
    assert_eq!(processor.hdr_metadata(9).unwrap().fields[8], 2000);
    let updated = br#"{"rtx_hdr_enable":true,"rtx_hdr_peak_brightness":1800,"rtx_hdr_middle_gray":60,"rtx_hdr_contrast":10,"rtx_hdr_saturation":-5,"rtx_hdr_debanding":false}"#;
    processor.update_stream(9, updated).unwrap();
    assert_eq!(processor.stream_settings[&9].peak_luminance, 1800);
    assert_eq!(processor.hdr_metadata(9).unwrap().fields[8], 2000);
    assert_eq!(
        processor.update_stream(9, b"{}"),
        Err(STATUS_INVALID_ARGUMENT)
    );
    assert_eq!(processor.stream_settings[&9].peak_luminance, 1800);
    assert_eq!(
        processor.update_stream(10, updated),
        Err(STATUS_INVALID_ARGUMENT)
    );
    let mut hijacked: serde_json::Value = serde_json::from_slice(updated).unwrap();
    hijacked["foreground_target"] = serde_json::json!({"kind":"stream","host_epoch":"epoch","session_id":"other","stream_id":"stream","generation":1});
    assert_eq!(
        processor.update_stream(9, &serde_json::to_vec(&hijacked).unwrap()),
        Err(STATUS_INVALID_ARGUMENT)
    );
    assert_eq!(processor.stream_settings[&9].peak_luminance, 1800);
    assert_eq!(processor.configure_stream(9, json), Err(MEDIA_BUSY));
    assert_eq!(processor.configure_stream(10, json), Err(MEDIA_BUSY));
    processor.cancel(9);
    assert_eq!(processor.configure_stream(9, json), Err(MEDIA_CLOSED));
    assert_eq!(processor.update_stream(9, updated), Err(MEDIA_CLOSED));
    processor.configure_stream(10, json).unwrap();
    processor.cancel(10);
}

#[test]
fn sdk_toggle_preserves_scope_metadata_and_atomic_rejection() {
    let (mut controls, _) = intent_settings();
    controls["rtx_hdr_enable"] = false.into();
    let mut processor = Processor::new(settings());
    processor
        .configure_stream(77, &serde_json::to_vec(&controls).unwrap())
        .unwrap();
    assert!(!processor.stream_settings[&77].enabled);
    let metadata = processor.hdr_metadata(77).unwrap();
    let target = processor.targets[&77].clone();
    let mut fresh = Processor::new(settings());
    fresh.device_uuid = Some([1; 16]);
    let raw = into_frame_processor_binding(fresh);
    // SAFETY: the SDK owns this live binding until its explicit release below.
    let api = unsafe { &*raw };
    unsafe { configure_frame_stream(api, 77, &serde_json::to_vec(&controls).unwrap()) }.unwrap();
    for enabled in [true, false, true] {
        controls["rtx_hdr_enable"] = enabled.into();
        assert_eq!(
            unsafe { update_frame_stream(api, 77, &serde_json::to_vec(&controls).unwrap()) },
            Ok(())
        );
        let mut invalid = controls.clone();
        invalid["rtx_hdr_enable"] = (!enabled).into();
        invalid["rtx_hdr_peak_brightness"] = 2001.into();
        assert_eq!(
            unsafe { update_frame_stream(api, 77, &serde_json::to_vec(&invalid).unwrap()) },
            Err(STATUS_INVALID_ARGUMENT)
        );
    }
    // Metadata remains HDR even while conversion is disabled; target ownership
    // cannot be replaced by a settings update.
    let mut hijack = controls.clone();
    hijack["foreground_target"]["generation"] = 2.into();
    assert!(
        unsafe { update_frame_stream(api, 77, &serde_json::to_vec(&hijack).unwrap()) }.is_err()
    );
    assert!(target.is_some());
    assert_eq!(metadata.enabled, 1);
    unsafe {
        (api.base.base.cancel)(api.base.base.base.context, 77);
        assert_eq!(
            update_frame_stream(api, 77, &serde_json::to_vec(&controls).unwrap()),
            Err(MEDIA_CLOSED)
        );
        api.base.base.base.release.unwrap()(api.base.base.base.context);
    }
}

const NONE: FrameDescriptor = FrameDescriptor {
    kind: FRAME_SYNC_NONE,
    version: 0,
    size: 0,
    reserved: 0,
    namespace: [0; 16],
    data: std::ptr::null(),
};
#[test]
fn input_negotiation_covers_cpu_and_both_rgb_depths_without_loading_vendor_code() {
    let offers = input::InputOffers::new();
    let (generation, list) = offers.enumerate();
    assert_eq!(list.len(), 14);
    for source in &list {
        let mut producer = *source;
        let mut dmabuf = None;
        if source.resource.kind == FRAME_RESOURCE_DMABUF {
            let mut caps = unsafe {
                source
                    .resource
                    .data
                    .cast::<DmaBufResourceCapabilities>()
                    .read_unaligned()
            };
            caps.modifier = 0x1234;
            dmabuf = Some(caps);
            producer.resource.data = std::ptr::from_ref(dmabuf.as_ref().unwrap()).cast();
        }
        assert!(offers.select(generation, source.id, &producer).is_ok());
        producer.ownership = FRAME_OWNERSHIP_BORROWED;
        assert!(offers.select(generation, source.id, &producer).is_err());
        let _ = dmabuf;
    }
}
#[test]
fn hdr_and_unknown_cpu_color_are_rejected_without_vendor_initialization() {
    let offers = input::InputOffers::new();
    let (generation, list) = offers.enumerate();
    let mut source = list[0];
    let mut caps = unsafe {
        source
            .resource
            .data
            .cast::<DmaBufResourceCapabilities>()
            .read_unaligned()
    };
    caps.color = DMABUF_COLOR_BT2020_PQ;
    source.resource.data = std::ptr::from_ref(&caps).cast();
    assert!(offers.select(generation, source.id, &source).is_err());
    let mut source = list[8];
    let mut caps = unsafe {
        source
            .resource
            .data
            .cast::<CpuResourceCapabilitiesV2>()
            .read_unaligned()
    };
    caps.color = FrameColorMetadata::default();
    source.resource.data = std::ptr::from_ref(&caps).cast();
    assert!(offers.select(generation, source.id, &source).is_err());
}
unsafe extern "C" {
    fn am_rtx_test_source_format(
        context: *mut c_void,
        frame: *mut DmaBufFrame,
        format: u32,
    ) -> *mut c_void;
    fn am_rtx_test_source_close(context: *mut c_void, source: *mut c_void);
    fn am_rtx_test_read_cuda(
        context: *mut c_void,
        pointer: u64,
        pitch: u32,
        pixels: *mut u32,
        length: usize,
    ) -> i32;
    fn am_rtx_test_fence(context: *mut c_void) -> i32;
}
struct Source {
    session: Rc<NativeSession>,
    raw: *mut c_void,
    frame: DmaBufFrame,
    next_frame: DmaBufFrame,
    references: Cell<usize>,
    fenced_releases: Cell<usize>,
}
impl Source {
    fn new(session: Rc<NativeSession>, format: u32) -> Box<Self> {
        let mut frame = frame();
        let raw = unsafe { am_rtx_test_source_format(session.0.as_ptr(), &mut frame, format) };
        assert!(!raw.is_null());
        frame.sequence = 1;
        frame.timestamp_us = 66;
        frame.planes[0].acquire_fence_fd = unsafe { am_rtx_test_fence(session.0.as_ptr()) };
        assert!(frame.planes[0].acquire_fence_fd >= 0);
        let mut next_frame = frame;
        next_frame.sequence = 2;
        Box::new(Self {
            session,
            raw,
            frame,
            next_frame,
            references: Cell::new(0),
            fenced_releases: Cell::new(0),
        })
    }
    fn offer(&self) -> (DmaBufResourceCapabilities, CaptureResourceOffer) {
        let caps = DmaBufResourceCapabilities {
            fourcc: self.frame.fourcc,
            color: self.frame.color,
            modifier: self.frame.modifier,
            min_width: self.frame.width,
            max_width: self.frame.width,
            min_height: self.frame.height,
            max_height: self.frame.height,
            device_id: [0; 16],
        };
        (
            caps,
            CaptureResourceOffer {
                id: 1,
                ownership: FRAME_OWNERSHIP_RETAINABLE,
                accessibility: FRAME_ACCESS_GPU,
                reserved: 0,
                resource: FrameDescriptor {
                    kind: FRAME_RESOURCE_DMABUF,
                    version: 1,
                    size: size_of::<DmaBufResourceCapabilities>() as u32,
                    reserved: 0,
                    namespace: [0; 16],
                    data: std::ptr::null(),
                },
                acquire_sync: NONE,
                completion_sync: FrameDescriptor {
                    kind: FRAME_SYNC_SYNC_FILE,
                    version: 1,
                    ..NONE
                },
            },
        )
    }
    fn resource(&self, sequence: u64, freshness: u32) -> CaptureResourceFrame {
        CaptureResourceFrame {
            offer_generation: 1,
            offer_id: 1,
            width: self.frame.width,
            height: self.frame.height,
            timestamp_us: 66,
            sequence,
            capture_pacing_timestamp_us: 99,
            freshness,
            reserved: 0,
            color: FrameColorMetadata {
                version: 1,
                size: size_of::<FrameColorMetadata>() as u32,
                primaries: FRAME_PRIMARIES_BT709,
                transfer: FRAME_TRANSFER_SRGB,
                matrix: FRAME_MATRIX_RGB,
                range: FRAME_RANGE_FULL,
            },
            clock: FrameClockMetadata::default(),
            pacing_clock: FrameClockMetadata::default(),
            resource: FrameDescriptor {
                kind: FRAME_RESOURCE_DMABUF,
                version: 1,
                size: size_of::<DmaBufFrame>() as u32,
                reserved: 0,
                namespace: [0; 16],
                data: std::ptr::from_ref(if sequence == 1 {
                    &self.frame
                } else {
                    &self.next_frame
                })
                .cast(),
            },
            acquire_sync: NONE,
            context: std::ptr::from_ref(self).cast_mut().cast(),
            retain: retain_source,
            release: release_source,
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        assert_eq!(self.references.get(), 0);
        unsafe {
            libc::close(self.frame.planes[0].acquire_fence_fd);
            am_rtx_test_source_close(self.session.0.as_ptr(), self.raw);
        }
    }
}
unsafe extern "C" fn retain_source(c: *mut c_void) -> Status {
    let source = unsafe { &*c.cast::<Source>() };
    source.references.set(source.references.get() + 1);
    STATUS_OK
}
unsafe extern "C" fn release_source(c: *mut c_void, d: *const FrameDescriptor) {
    let source = unsafe { &*c.cast::<Source>() };
    let d = unsafe { &*d };
    if d.kind == FRAME_SYNC_SYNC_FILE {
        let fd = unsafe { (*d.data.cast::<FrameSyncFile>()).fd };
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut p, 1, 1000) } > 0);
        source.fenced_releases.set(source.fenced_releases.get() + 1);
    } else {
        assert_eq!(d.kind, FRAME_SYNC_NONE);
    }
    source
        .references
        .set(source.references.get().checked_sub(1).unwrap());
}
struct Binding(*const FrameProcessorApi);
impl Binding {
    fn api(&self) -> &FrameProcessorApi {
        unsafe { &*self.0 }
    }
    fn new(config: Settings) -> Self {
        let b = Self(into_frame_processor_binding(Processor::new(config)));
        let (controls, _) = intent_settings();
        unsafe { configure_frame_stream(b.api(), 77, &serde_json::to_vec(&controls).unwrap()) }
            .unwrap();
        b
    }
    fn negotiate(&self, source: &CaptureResourceOffer) {
        let api = self.api();
        let context = api.base.base.base.context;
        let mut inputs = Vec::<u64>::new();
        let mut generation = 0;
        unsafe extern "C" fn collect(c: *mut c_void, o: *const CaptureResourceOffer) -> Status {
            unsafe { &mut *c.cast::<Vec<u64>>() }.push(unsafe { (*o).id });
            STATUS_OK
        }
        assert_eq!(
            unsafe {
                (api.base.base.enumerate_resources)(
                    context,
                    77,
                    CaptureResourceOfferSink {
                        context: std::ptr::from_mut(&mut inputs).cast(),
                        write: collect,
                    },
                    &mut generation,
                )
            },
            STATUS_OK
        );
        assert!(inputs.into_iter().any(|id| unsafe {
            (api.base.base.configure)(context, 77, generation, id, 1, source)
        } == STATUS_OK));
        let mut outputs = Vec::<u64>::new();
        assert_eq!(
            unsafe {
                (api.base.enumerate_outputs)(
                    context,
                    77,
                    CaptureResourceOfferSink {
                        context: std::ptr::from_mut(&mut outputs).cast(),
                        write: collect,
                    },
                    &mut generation,
                )
            },
            STATUS_OK
        );
        assert_eq!(
            unsafe { (api.base.select_output)(context, 77, generation, outputs[0]) },
            STATUS_OK
        );
    }
    fn process(&self, frame: &CaptureResourceFrame) -> Result<CaptureResourceFrame, Status> {
        let mut output = None;
        unsafe extern "C" fn receive(c: *mut c_void, f: *const CaptureResourceFrame) -> Status {
            let frame = unsafe { &*f };
            let status = unsafe { (frame.retain)(frame.context) };
            if status == STATUS_OK {
                unsafe { &mut *c.cast::<Option<CaptureResourceFrame>>() }.replace(*frame);
            }
            status
        }
        let api = self.api();
        let result = unsafe {
            (api.base.process)(
                api.base.base.base.context,
                77,
                frame,
                CaptureResourceFrameSink {
                    context: std::ptr::from_mut(&mut output).cast(),
                    write: receive,
                },
            )
        };
        if result == STATUS_OK {
            Ok(output.unwrap())
        } else {
            assert!(output.is_none());
            Err(result)
        }
    }
}
impl Drop for Binding {
    fn drop(&mut self) {
        let api = self.api();
        unsafe {
            (api.base.base.cancel)(api.base.base.base.context, 77);
            api.base.base.base.release.unwrap()(api.base.base.base.context);
        }
    }
}
fn gpu_settings() -> Settings {
    let mut s = settings();
    s.runtime_directory = std::env::var_os("AMATERASU_TRUEHDR_RUNTIME")
        .expect("licensed runtime path")
        .into();
    s.output_buffers = 1;
    s
}
fn read(session: &NativeSession, frame: &CaptureResourceFrame) -> Vec<u32> {
    assert_eq!(frame.resource.kind, FRAME_RESOURCE_CUDA_DEVICE);
    assert_eq!(frame.acquire_sync.kind, FRAME_SYNC_CUDA_EVENT);
    let cuda = unsafe { *frame.resource.data.cast::<CudaDeviceFrame>() };
    let mut pixels = vec![0; frame.width as usize * frame.height as usize];
    assert_eq!(
        unsafe {
            am_rtx_test_read_cuda(
                session.0.as_ptr(),
                cuda.device_pointer,
                cuda.pitch,
                pixels.as_mut_ptr(),
                pixels.len(),
            )
        },
        0
    );
    pixels
}
unsafe fn release(frame: CaptureResourceFrame) {
    unsafe { (frame.release)(frame.context, &NONE) };
}
#[test]
#[ignore = "requires NVIDIA RTX and AMATERASU_TRUEHDR_RUNTIME; run only with the stream idle"]
fn nvidia_dma_inputs_produce_hdr_pixels_and_one_buffer_backpressure_without_copying_output() {
    for format in [*b"AB24", *b"AB30"] {
        let config = gpu_settings();
        let fixture = Stream::open(&config, 512, 288, None).unwrap();
        let source = Source::new(fixture.session.clone(), u32::from_le_bytes(format));
        let binding = Binding::new(config);
        let (caps, mut offer) = source.offer();
        offer.resource.data = std::ptr::from_ref(&caps).cast();
        binding.negotiate(&offer);
        let first = binding
            .process(&source.resource(1, CAPTURE_FRAME_FRESH))
            .unwrap();
        assert_eq!(first.color.transfer, FRAME_TRANSFER_PQ);
        assert_eq!(first.capture_pacing_timestamp_us, 99);
        let pixels = read(&source.session, &first);
        assert_ne!(pixels[0] & 0x3fff_ffff, pixels[511] & 0x3fff_ffff);
        // A separate reader pins the immutable output while another step runs.
        assert!(matches!(
            binding.process(&source.resource(2, CAPTURE_FRAME_FRESH)),
            Err(MEDIA_BUSY)
        ));
        assert_eq!(read(&source.session, &first), pixels);
        unsafe { release(first) };
        let second = binding
            .process(&source.resource(2, CAPTURE_FRAME_FRESH))
            .unwrap();
        let after = read(&source.session, &second);
        unsafe { release(second) };
        let cached = binding
            .process(&source.resource(2, CAPTURE_FRAME_CACHED_REUSE))
            .unwrap();
        assert_eq!(cached.freshness, CAPTURE_FRAME_CACHED_REUSE);
        assert_eq!(read(&source.session, &cached), after);
        unsafe { release(cached) };
        drop(binding);
        assert_eq!(source.references.get(), 0);
        assert!(source.fenced_releases.get() > 0);
    }
}
#[test]
#[ignore = "requires NVIDIA RTX and AMATERASU_TRUEHDR_RUNTIME; run only with the stream idle"]
fn nvidia_cpu_bgra_nv12_and_ten_bit_sdr_produce_cuda_hdr_and_retire_source_references() {
    use amaterasu_module_sdk::capture::{CpuFrameLeasePool, CpuSourcePresentation};
    use std::sync::Arc;
    for (format, id) in [(PIXEL_BGRA8, 9), (PIXEL_NV12, 11), (PIXEL_P010, 13)] {
        let config = gpu_settings();
        let fixture = Stream::open(&config, 256, 144, None).unwrap();
        let binding = Binding::new(config);
        let inputs = input::InputOffers::new();
        let (_, offers) = inputs.enumerate();
        let mut offer = offers.into_iter().find(|o| o.id == id).unwrap();
        offer.id = 1;
        binding.negotiate(&offer);
        let caps = unsafe { *offer.resource.data.cast::<CpuResourceCapabilitiesV2>() };
        let stride = 256
            * if format == PIXEL_BGRA8 {
                4
            } else if format == PIXEL_P010 {
                2
            } else {
                1
            };
        let rows = if format == PIXEL_BGRA8 { 144 } else { 216 };
        let mut bytes = vec![128; stride as usize * rows];
        if format == PIXEL_P010 {
            for pixel in bytes.as_chunks_mut::<2>().0 {
                pixel.copy_from_slice(&(512u16 << 6).to_le_bytes());
            }
        }
        let pool = CpuFrameLeasePool::default();
        let lease = pool
            .lease(
                CpuSourcePresentation {
                    receipt: None,
                    width: 256,
                    height: 144,
                    stride,
                    pixel_format: format,
                    color: caps.color,
                    bytes: Arc::new(bytes).into(),
                    timestamp_us: 66,
                    capture_pacing_timestamp_us: 99,
                    sequence: 1,
                    freshness: CAPTURE_FRAME_FRESH,
                },
                1,
                1,
            )
            .unwrap();
        let output = binding.process(lease.descriptor()).unwrap();
        drop(lease);
        assert_eq!(pool.retirement_ready(), Ok(false));
        assert!(
            read(&fixture.session, &output)
                .iter()
                .any(|p| p & 0x3fff_ffff != 0)
        );
        unsafe { release(output) };
        drop(binding);
        assert_eq!(pool.retirement_ready(), Ok(true));
    }
}

#[test]
#[ignore = "requires NVIDIA RTX and AMATERASU_TRUEHDR_RUNTIME; run only with the stream idle"]
fn nvidia_cached_output_is_invalidated_by_updates_without_mutating_other_readers() {
    let mut config = gpu_settings();
    config.output_buffers = 2;
    let fixture = Stream::open(&config, 512, 288, None).unwrap();
    let source = Source::new(fixture.session.clone(), u32::from_le_bytes(*b"AB24"));
    let binding = Binding::new(config);
    let (caps, mut offer) = source.offer();
    offer.resource.data = std::ptr::from_ref(&caps).cast();
    binding.negotiate(&offer);
    let original = binding
        .process(&source.resource(1, CAPTURE_FRAME_FRESH))
        .unwrap();
    let before = read(&source.session, &original);
    let cached = binding
        .process(&source.resource(1, CAPTURE_FRAME_CACHED_REUSE))
        .unwrap();
    assert_eq!(cached.resource.data, original.resource.data);
    assert_eq!(read(&source.session, &cached), before);
    unsafe { release(cached) };
    let api = binding.api();
    let (mut controls, _) = intent_settings();
    controls["rtx_hdr_enable"] = false.into();
    unsafe { update_frame_stream(api, 77, &serde_json::to_vec(&controls).unwrap()) }.unwrap();
    let changed = binding
        .process(&source.resource(1, CAPTURE_FRAME_CACHED_REUSE))
        .unwrap();
    assert_eq!(changed.color.transfer, FRAME_TRANSFER_PQ);
    let after = read(&source.session, &changed);
    assert_ne!(before, after);
    assert_eq!(read(&source.session, &original), before);
    // The disabled effect still emits the HDR stream's SDR-compatible PQ path.
    let code = f64::from(after[300] & 1023) / 1023.0;
    let p = code.powf(32.0 / 2523.0);
    let nits =
        10000.0 * ((p - 0.8359375).max(0.0) / (18.8515625 - 18.6875 * p)).powf(16384.0 / 2610.0);
    assert!((nits - 100.0).abs() < 3.0);
    // Stream cancellation leaves other readers valid while the binding stays
    // loaded; release those readers before returning the native binding.
    unsafe { (api.base.base.cancel)(api.base.base.base.context, 77) };
    assert_eq!(retirement_ready(), Ok(false));
    assert_eq!(read(&source.session, &original), before);
    unsafe {
        release(changed);
        release(original);
    }
    assert_eq!(source.references.get(), 0);
    drop(binding);
    drop(source);
    drop(fixture);
    assert_eq!(retirement_ready(), Ok(true));
}

#[test]
fn cuda_admission_limits_recover_without_waiting_or_wraparound() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    assert_eq!(resource::validate_storage_budget(3840, 2160, 2), Ok(()));
    assert_eq!(
        resource::validate_storage_budget(8192, 8192, 2),
        Err(MEDIA_UNSUPPORTED)
    );
    let count = AtomicUsize::new(0);
    assert_eq!(resource::reserve(&count, 64, 64), Ok(()));
    assert_eq!(resource::reserve(&count, 1, 64), Err(MEDIA_BUSY));
    assert_eq!(resource::reserve(&count, usize::MAX, 64), Err(MEDIA_BUSY));
    count.fetch_sub(64, Ordering::AcqRel);
    assert_eq!(resource::reserve(&count, 1, 64), Ok(()));
}

#[test]
fn contended_foreground_policy_defers_without_switching_effects_or_waiting() {
    use std::sync::{Arc, Mutex, atomic::AtomicUsize};
    let policy = Arc::new(Mutex::new(
        crate::foreground_policy::ForegroundPolicy::default(),
    ));
    let mut config = settings();
    config.foreground_only = true;
    let processor =
        Processor::new(config).with_policy(policy.clone(), Arc::new(AtomicUsize::new(0)));
    let held = policy.lock().unwrap();
    assert_eq!(processor.conversion_enabled(1), Err(MEDIA_BUSY));
    drop(held);
    assert_eq!(processor.conversion_enabled(1), Ok(false));
}

#[test]
fn graphics_input_is_released_before_inference_and_on_copy_failure() {
    unsafe extern "C" {
        fn am_rtx_test_input_copy_scope(failure: u32) -> i32;
    }
    for failure in 0..=2 {
        // SAFETY: the private fixture uses thread-local fake CUDA callbacks;
        // it owns its test API and retains no pointers after this call.
        assert_eq!(unsafe { am_rtx_test_input_copy_scope(failure) }, 0);
    }
}
