use super::*;
use amaterasu_module_sdk::processing::{
    ProcessingOutcome, configure_gpu_stream, into_gpu_binding, process_gpu, update_gpu_stream,
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
    let request = serde_json::json!({"method":"processing_intent","interface_major":1,"interface_minor":0,"arguments":{"target":target,"revision":"snapshot","settings":settings}});
    let response = crate::lifecycle::intent("consumer", &["consumer".into()], request);
    assert_eq!(response["status"], "succeeded");
    (response["processor_settings"].clone(), settings)
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
fn packed_rgb_sdr_accepts_both_scanout_depths_without_accepting_hdr() {
    for format in [
        *b"XR24", *b"AR24", *b"XB24", *b"AB24", *b"XR30", *b"AR30", *b"XB30", *b"AB30",
    ] {
        let mut input = frame();
        input.fourcc = u32::from_le_bytes(format);
        assert!(supported_input(&input));
        input.color = DMABUF_COLOR_BT2020_PQ;
        assert!(!supported_input(&input));
    }
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
    let raw = into_gpu_binding(processor);
    // SAFETY: the SDK owns this live binding until its explicit release below.
    let api = unsafe { &*raw };
    for enabled in [true, false, true] {
        controls["rtx_hdr_enable"] = enabled.into();
        assert_eq!(
            unsafe { update_gpu_stream(api, 77, &serde_json::to_vec(&controls).unwrap()) },
            Ok(())
        );
        let mut invalid = controls.clone();
        invalid["rtx_hdr_enable"] = (!enabled).into();
        invalid["rtx_hdr_peak_brightness"] = 2001.into();
        assert_eq!(
            unsafe { update_gpu_stream(api, 77, &serde_json::to_vec(&invalid).unwrap()) },
            Err(STATUS_INVALID_ARGUMENT)
        );
    }
    // Metadata remains HDR even while conversion is disabled; target ownership
    // cannot be replaced by a settings update.
    let mut hijack = controls.clone();
    hijack["foreground_target"]["generation"] = 2.into();
    assert!(unsafe { update_gpu_stream(api, 77, &serde_json::to_vec(&hijack).unwrap()) }.is_err());
    assert!(target.is_some());
    assert_eq!(metadata.enabled, 1);
    unsafe {
        (api.cancel_dmabuf_leases)(api.base.context, 77);
        assert_eq!(
            update_gpu_stream(api, 77, &serde_json::to_vec(&controls).unwrap()),
            Err(MEDIA_CLOSED)
        );
        api.base.release.unwrap()(api.base.context);
    }
}
unsafe extern "C" fn input_retain(_: *mut c_void) -> Status {
    STATUS_OK
}
unsafe extern "C" fn input_release(_: *mut c_void, _: i32) {}

unsafe extern "C" {
    fn am_rtx_test_source(context: *mut c_void, frame: *mut DmaBufFrame) -> *mut c_void;
    fn am_rtx_test_source_format(
        context: *mut c_void,
        frame: *mut DmaBufFrame,
        format: u32,
    ) -> *mut c_void;
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
        Self::new_format(session, None)
    }
    fn new_format(session: Rc<NativeSession>, format: Option<u32>) -> Self {
        let mut frame = frame();
        let raw = unsafe {
            match format {
                Some(format) => am_rtx_test_source_format(session.0.as_ptr(), &mut frame, format),
                None => am_rtx_test_source(session.0.as_ptr(), &mut frame),
            }
        };
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
    pixels
}

fn nits(code: u32) -> f64 {
    let p = (f64::from(code) / 1023.0).powf(32.0 / 2523.0);
    10000.0 * ((p - 0.8359375).max(0.0) / (18.8515625 - 18.6875 * p)).powf(16384.0 / 2610.0)
}

#[test]
#[ignore = "requires an NVIDIA RTX GPU and AMATERASU_TRUEHDR_RUNTIME pointing to the licensed VFX runtime"]
fn nvidia_sdk_toggle_from_disabled_preserves_frames_and_hdr_output() {
    for scope in ["global", "application"] {
        verify_gpu_toggle(scope, None);
    }
}

#[test]
#[ignore = "requires an NVIDIA RTX GPU and AMATERASU_TRUEHDR_RUNTIME pointing to the licensed VFX runtime"]
fn nvidia_ten_bit_sdr_application_converts_and_preserves_held_frames() {
    for format in [*b"AR30", *b"AB30"] {
        verify_gpu_toggle("application", Some(u32::from_le_bytes(format)));
    }
}

fn verify_gpu_toggle(scope: &str, format: Option<u32>) {
    let mut config = settings();
    config.runtime_directory = std::env::var_os("AMATERASU_TRUEHDR_RUNTIME")
        .expect("runtime path")
        .into();
    config.output_buffers = 2;
    let stream = Stream::open(&config, 512, 288, None).unwrap();
    let source = Source::new_format(stream.slots[0].session.clone(), format);
    let mut processor = Processor::new(config);
    let (mut controls, _) = intent_settings();
    controls["rtx_hdr_enable"] = false.into();
    controls["rtx_hdr_scope"] = scope.into();
    let enable_key = if scope == "application" {
        "rtx_hdr_application_enable"
    } else {
        "rtx_hdr_enable"
    };
    processor
        .configure_stream(77, &serde_json::to_vec(&controls).unwrap())
        .unwrap();
    let metadata = processor.hdr_metadata(77).unwrap();
    let raw = into_gpu_binding(processor);
    // SAFETY: this test retains the SDK-owned binding and its input source until
    // all output references have been released and cancellation has completed.
    let api = unsafe { &*raw };
    let input = source.lease();
    let mut held = None;
    let mut fallback = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                assert_eq!(output.frame.color, DMABUF_COLOR_BT2020_PQ);
                fallback = read(output);
                assert_eq!(output.retain.unwrap()(output.context), STATUS_OK);
                held = Some(*output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    let held = held.unwrap();
    assert!((nits(fallback[300] & 1023) - 100.0).abs() < 3.0);
    let mut converted = None;
    for enabled in [true, false, true, false] {
        controls[enable_key] = enabled.into();
        assert_eq!(
            unsafe { update_gpu_stream(api, 77, &serde_json::to_vec(&controls).unwrap()) },
            Ok(())
        );
        let mut pixels = Vec::new();
        assert_eq!(
            unsafe {
                process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                    assert_eq!(output.frame.color, held.frame.color);
                    assert_eq!(output.frame.fourcc, held.frame.fourcc);
                    assert_eq!(output.frame.sequence, input.frame.sequence);
                    pixels = read(output);
                    STATUS_OK
                })
            },
            ProcessingOutcome::Accepted
        );
        let white = nits(pixels[300] & 1023);
        if enabled {
            assert!(white > 900.0, "conversion white: {white}");
            if let Some(expected) = &converted {
                assert_eq!(&pixels, expected, "conversion changed on repeated enable");
            } else {
                converted = Some(pixels);
            }
        } else {
            assert_eq!(pixels, fallback);
        }
        assert_eq!(read(&held), fallback, "toggle changed a retained frame");
        let mut observed = metadata;
        let v1_1 = unsafe { &*raw.cast::<amaterasu_media::GpuProcessorApiV1_1>() };
        assert_eq!(
            unsafe { (v1_1.hdr_metadata)(api.base.context, 77, &mut observed) },
            STATUS_OK
        );
        assert_eq!(observed.fields, metadata.fields);
        assert_eq!(observed.enabled, metadata.enabled);
    }
    unsafe {
        held.release.unwrap()(held.context, -1);
        (api.cancel_dmabuf_leases)(api.base.context, 77);
        api.base.release.unwrap()(api.base.context);
    }
}

#[test]
#[ignore = "requires an NVIDIA RTX GPU and AMATERASU_TRUEHDR_RUNTIME pointing to the licensed VFX runtime"]
fn nvidia_live_tuning_preserves_held_frames_and_calibrates_fallback() {
    let mut config = settings();
    config.runtime_directory = std::env::var_os("AMATERASU_TRUEHDR_RUNTIME")
        .expect("runtime path")
        .into();
    config.output_buffers = 2;
    config.peak_luminance = 1000;
    let stream = Stream::open(&config, 512, 288, None).unwrap();
    let session = stream.slots[0].session.clone();
    let source = Source::new(session.clone());
    let mut processor = Processor::new(config.clone());
    let (initial_controls, _) = intent_settings();
    processor
        .configure_stream(77, &serde_json::to_vec(&initial_controls).unwrap())
        .unwrap();
    processor.streams.insert(77, stream);
    let raw = into_gpu_binding(processor);
    let api = unsafe { &*raw };
    let input = source.lease();
    let mut held = None;
    let mut initial = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                initial = read(output);
                assert_eq!(output.retain.unwrap()(output.context), STATUS_OK);
                held = Some(*output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    let held = held.unwrap();
    let mut vendor = vec![0; initial.len()];
    assert_eq!(
        unsafe { am_rtx_test_read_vendor(session.0.as_ptr(), vendor.as_mut_ptr(), vendor.len()) },
        0
    );
    assert!(
        initial
            .iter()
            .zip(&vendor)
            .all(|(a, b)| a & 0x3fff_ffff == b & 0x3fff_ffff)
    );

    config.peak_luminance = 2000;
    let live = br#"{"rtx_hdr_enable":true,"rtx_hdr_peak_brightness":2000,"rtx_hdr_middle_gray":50,"rtx_hdr_contrast":0,"rtx_hdr_saturation":0,"rtx_hdr_debanding":true}"#;
    assert_eq!(unsafe { update_gpu_stream(api, 77, live) }, Ok(()));
    let mut peak = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                peak = read(output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    assert_eq!(
        read(&held),
        initial,
        "reconfiguration must preserve leased pixels"
    );
    let highlight = nits(peak[300] & 1023);
    eprintln!(
        "2000-nit setting: white={highlight:.1} nits; baseline={:.1} nits",
        nits(initial[300] & 1023)
    );
    assert!(
        (highlight / 2000.0 - 1.0).abs() < 0.08,
        "peak calibration did not reach target"
    );
    assert!(
        nits(peak[511] & 1023) < 0.01,
        "calibration must preserve black"
    );
    for requested_peak in [1200, 1600, 2000] {
        for requested_gray in [10, 50] {
            config.peak_luminance = requested_peak;
            config.middle_gray = requested_gray;
            Stream::configure_session(&session, &config, true, None).unwrap();
            let mut sample = Vec::new();
            assert_eq!(
                unsafe {
                    process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                        sample = read(output);
                        STATUS_OK
                    })
                },
                ProcessingOutcome::Accepted
            );
            let white = nits(sample[300] & 1023);
            eprintln!("peak={requested_peak}, gray={requested_gray}: white={white:.1} nits");
            assert!((white / f64::from(requested_peak) - 1.0).abs() < 0.08);
            assert!(
                sample
                    .iter()
                    .all(|p| [0, 10, 20].iter().all(|shift| (p >> shift) & 1023 <= 846))
            );
            assert!(nits(sample[511] & 1023) < 0.01);
        }
    }
    let rejected = br#"{"rtx_hdr_enable":true,"rtx_hdr_peak_brightness":2001,"rtx_hdr_middle_gray":50,"rtx_hdr_contrast":0,"rtx_hdr_saturation":0,"rtx_hdr_debanding":true}"#;
    assert_eq!(
        unsafe { update_gpu_stream(api, 77, rejected) },
        Err(STATUS_INVALID_ARGUMENT)
    );
    let invalid = [201u32, 100, 50, 1000, 0];
    let mut error = [0; 512];
    assert_ne!(
        unsafe {
            am_rtx_configure(
                session.0.as_ptr(),
                invalid.as_ptr(),
                false,
                0,
                error.as_mut_ptr(),
                error.len(),
            )
        },
        0
    );
    let mut after_rejection = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                after_rejection = read(output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    assert!(
        nits(after_rejection[300] & 1023) > 1900.0,
        "rejected update changed the active conversion"
    );
    config.peak_luminance = 1000;
    for control in ["contrast", "saturation", "middle_gray", "debanding"] {
        let mut changed = config.clone();
        match control {
            "contrast" => changed.contrast = 0,
            "saturation" => changed.saturation = 0,
            "middle_gray" => changed.middle_gray = 10,
            "debanding" => changed.debanding = false,
            _ => unreachable!(),
        }
        Stream::configure_session(&session, &changed, true, None).unwrap();
        let mut sample = Vec::new();
        assert_eq!(
            unsafe {
                process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                    sample = read(output);
                    STATUS_OK
                })
            },
            ProcessingOutcome::Accepted
        );
        if control != "debanding" {
            assert_ne!(sample, initial, "live {control} must alter sampled pixels");
        }
    }
    Stream::configure_session(&session, &config, false, None).unwrap();
    let mut fallback_100 = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                fallback_100 = read(output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    config.sdr_brightness = 100;
    Stream::configure_session(&session, &config, false, None).unwrap();
    let mut fallback_200 = Vec::new();
    assert_eq!(
        unsafe {
            process_gpu(api, 77, &input, CAPTURE_FRAME_FRESH, |output| {
                fallback_200 = read(output);
                STATUS_OK
            })
        },
        ProcessingOutcome::Accepted
    );
    assert!((nits(fallback_100[300] & 1023) - 100.0).abs() < 3.0);
    assert!((nits(fallback_200[300] & 1023) - 200.0).abs() < 5.0);
    assert_eq!(read(&held), initial);
    unsafe {
        held.release.unwrap()(held.context, -1);
        (api.cancel_dmabuf_leases)(api.base.context, 77);
        api.base.release.unwrap()(api.base.context);
    }
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
