use super::*;
use amaterasu_module_sdk::processing::{
    ProcessingOutcome, configure_gpu_stream, into_gpu_binding, process_gpu,
};

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

#[test]
fn unsupported_input_is_rejected_before_loading_vendor_code() {
    let mut processor = Processor::new(settings());
    let mut source = frame();
    for change in 0..4 {
        match change {
            0 => source.color = DMABUF_COLOR_BT2020_PQ,
            1 => {
                source.color = DMABUF_COLOR_SRGB;
                source.fourcc = u32::from_le_bytes(*b"NV12");
            }
            2 => {
                source.fourcc = u32::from_le_bytes(*b"XR24");
                source.width = 8;
            }
            _ => {
                source.width = 256;
                source.plane_count = 2;
            }
        }
        assert!(!supported_input(&source));
    }
    processor.cancel(7);
    assert!(processor.retired.contains(&7));
    // A cancelled identity cannot create a new native stream.
    let raw = into_gpu_binding(processor);
    unsafe {
        let api = &*raw;
        let input = DmaBufFrameLease {
            frame: frame(),
            context: std::ptr::dangling_mut::<u8>().cast(),
            retain: Some(input_retain),
            release: Some(input_release),
        };
        assert_eq!(
            process_gpu(api, 7, &input, CAPTURE_FRAME_FRESH, |_| panic!(
                "unexpected delivery"
            )),
            ProcessingOutcome::Failed(MEDIA_CLOSED)
        );
        api.base.release.unwrap()(api.base.context);
    }
}
#[test]
fn frozen_stream_controls_precede_metadata_and_retire_with_stream() {
    let mut processor = Processor::new(settings());
    let json = br#"{"rtx_hdr_enable":true,"rtx_hdr_peak_brightness":1400,"rtx_hdr_middle_gray":50,"rtx_hdr_contrast":5,"rtx_hdr_saturation":-5,"rtx_hdr_debanding":true}"#;
    processor.configure_stream(9, json).unwrap();
    assert_eq!(processor.hdr_metadata(9).unwrap().fields[8], 1400);
    assert_eq!(processor.configure_stream(9, json), Err(MEDIA_BUSY));
    assert_eq!(processor.configure_stream(10, json), Err(MEDIA_BUSY));
    processor.cancel(9);
    assert_eq!(processor.configure_stream(9, json), Err(MEDIA_CLOSED));
    processor.configure_stream(10, json).unwrap();
    processor.cancel(10);
}
unsafe extern "C" fn input_retain(_: *mut c_void) -> Status {
    STATUS_OK
}
unsafe extern "C" fn input_release(_: *mut c_void, _: i32) {}

unsafe extern "C" {
    fn am_rtx_test_source(context: *mut c_void, frame: *mut DmaBufFrame) -> *mut c_void;
    fn am_rtx_test_read(
        context: *mut c_void,
        output: *mut c_void,
        pixels: *mut u32,
        length: usize,
    ) -> i32;
    fn am_rtx_test_read_vendor(context: *mut c_void, pixels: *mut u32, length: usize) -> i32;
    fn am_rtx_test_fence(context: *mut c_void) -> i32;
}
struct Source {
    session: Rc<NativeSession>,
    raw: *mut c_void,
    frame: DmaBufFrame,
}
impl Source {
    fn new(session: Rc<NativeSession>) -> Self {
        let mut frame = frame();
        let raw = unsafe { am_rtx_test_source(session.0.as_ptr(), &mut frame) };
        assert!(!raw.is_null(), "GPU test source allocation failed");
        frame.timestamp_us = 66;
        frame.sequence = 1;
        frame.planes[0].acquire_fence_fd = unsafe { am_rtx_test_fence(session.0.as_ptr()) };
        assert!(
            frame.planes[0].acquire_fence_fd >= 0,
            "native acquire fence"
        );
        Self {
            session,
            raw,
            frame,
        }
    }
    fn lease(&self) -> DmaBufFrameLease {
        DmaBufFrameLease {
            frame: self.frame,
            context: std::ptr::from_ref(self).cast_mut().cast(),
            retain: Some(input_retain),
            release: Some(input_release),
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.frame.planes[0].acquire_fence_fd);
            am_rtx_output_close(self.session.0.as_ptr(), self.raw);
        }
    }
}
fn read(lease: &DmaBufFrameLease) -> Vec<u32> {
    let state = unsafe { &*lease.context.cast::<Lease>() };
    let mut pixels = vec![0; lease.frame.width as usize * lease.frame.height as usize];
    assert_eq!(
        unsafe {
            am_rtx_test_read(
                state.slot.session.0.as_ptr(),
                state.slot.output.as_ptr(),
                pixels.as_mut_ptr(),
                pixels.len(),
            )
        },
        0
    );
    let mut vendor = vec![0; pixels.len()];
    assert_eq!(
        unsafe {
            am_rtx_test_read_vendor(
                state.slot.session.0.as_ptr(),
                vendor.as_mut_ptr(),
                vendor.len(),
            )
        },
        0
    );
    assert!(
        pixels
            .iter()
            .zip(&vendor)
            .all(|(exported, native)| exported & 0x3fff_ffff == native & 0x3fff_ffff),
        "exported RGB codes differ from vendor output: export={:#x}, vendor={:#x}",
        pixels[0],
        vendor[0]
    );
    pixels
}

#[test]
#[ignore = "requires an NVIDIA RTX GPU and AMATERASU_TRUEHDR_RUNTIME pointing to the licensed VFX runtime"]
fn nvidia_gpu_processes_real_pixels_and_preserves_bounded_ownership() {
    let mut config = settings();
    config.runtime_directory = std::env::var_os("AMATERASU_TRUEHDR_RUNTIME")
        .expect("runtime path")
        .into();
    config.output_buffers = 1;
    let stream = Stream::open(&config, 512, 288, None).unwrap();
    let source = Source::new(stream.slots[0].session.clone());
    let mut processor = Processor::new(config.clone());
    processor.streams.insert(77, stream);
    let raw = into_gpu_binding(processor);
    let api = unsafe { &*raw };
    let input = source.lease();
    let mut held = None;
    let mut first = Vec::new();
    let outcome = unsafe {
        process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
            assert_eq!(output.frame.color, DMABUF_COLOR_BT2020_PQ);
            assert_eq!(output.frame.fourcc, u32::from_le_bytes(*b"AB30"));
            first = read(output);
            assert_eq!(output.retain.unwrap()(output.context), STATUS_OK);
            held = Some(*output);
            STATUS_OK
        })
    };
    assert_eq!(outcome, ProcessingOutcome::Accepted);
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |_| {
                panic!("pool must be full")
            })
        },
        ProcessingOutcome::Busy
    );
    let held = held.take().unwrap();
    let completion = unsafe { am_rtx_test_fence(source.session.0.as_ptr()) };
    assert!(completion >= 0);
    unsafe {
        held.release.unwrap()(held.context, completion);
    }
    assert_eq!(
        unsafe { libc::fcntl(completion, libc::F_GETFD) },
        -1,
        "release owns and closes completion fd"
    );
    let mut repeated = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_CACHED_REUSE, |output| {
                repeated = read(output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    assert!(
        first == repeated,
        "same pixels/settings must be deterministic: first={:#x}, repeated={:#x}, changed={}",
        first[0],
        repeated[0],
        first.iter().zip(&repeated).filter(|(a, b)| a != b).count()
    );
    let bright = first[0];
    let black = first[511];
    assert_ne!(
        bright & 0x3fff_ffff,
        black & 0x3fff_ffff,
        "actual HDR pixels must change with source content"
    );
    assert!(
        bright & 1023 > (bright >> 20) & 1023,
        "warm input must retain red over blue"
    );
    unsafe {
        (api.cancel_dmabuf_leases)(api.base.context, 77);
    }
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |_| {
                panic!("cancelled")
            })
        },
        ProcessingOutcome::Failed(MEDIA_CLOSED)
    );
    unsafe {
        api.base.release.unwrap()(api.base.context);
    }

    let raw = into_gpu_binding(Processor::new(config));
    let api = unsafe { &*raw };
    let stream_settings = br#"{"rtx_hdr_enable":true,"rtx_hdr_peak_brightness":650,"rtx_hdr_middle_gray":50,"rtx_hdr_contrast":0,"rtx_hdr_saturation":-100,"rtx_hdr_debanding":true}"#;
    unsafe { configure_gpu_stream(api, 88, stream_settings) }.unwrap();
    let mut changed = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 88, &input, CAPTURE_FRAME_FRESH, |output| {
                changed = read(output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    assert!(
        first != changed,
        "vendor saturation control must alter numeric HDR output"
    );
    eprintln!(
        "TrueHDR numeric sample: warm={bright:#010x}, black={black:#010x}, saturation0={:#010x}",
        changed[0]
    );
    unsafe {
        (api.cancel_dmabuf_leases)(api.base.context, 88);
        api.base.release.unwrap()(api.base.context);
    }
}
