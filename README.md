# RTX HDR plugin

`amaterasu.rtx-hdr/1` converts captured SDR pixels to HDR10 with NVIDIA's native
TrueHDR effect. It is an independently compiled GPU post-processing provider.
Video invokes it through the public Processing SDK; the plugin does not drive
capture, call video, select an encoder, or alter the host shell.

The current backend supports 64-bit Linux on NVIDIA RTX, using the NVIDIA VFX
1.3.0 runtime from the `nvidia-vfx` 0.2.0.0 wheel. Vendor libraries are not part
of this repository or the default installation. Amaterasu's optional Web UI
installation downloads and verifies the required files under NVIDIA's license.
The runtime directory is an explicit trusted startup setting, independent of
Vibeshine's installation. The documented NVIDIA minimum driver is 570.

## Build and composition

The current source is built in the Amaterasu workspace, which supplies its SDK,
runtime verifier, and native headers. Check out this repository as the
`modules/rtx-hdr` submodule and build from the workspace root with
`cargo build -p amaterasu-rtx-hdr --features linux-nvidia`. A standalone SDK
build is not yet provided. The
resulting shared library is `target/debug/libamaterasu_rtx_hdr.so`. Development
headers for CUDA, EGL, OpenGL, GBM and DRM are required. CUDA headers default to
`/opt/cuda/include`; set `AMATERASU_CUDA_INCLUDE_DIR` when needed. CUDA and VFX
functions are resolved at runtime. A build without this feature refuses startup
instead of registering a substitute effect.

Example plugin startup settings:

```json
{
  "runtime_directory": "/opt/amaterasu/nvidia-vfx-0.2.0.0",
  "device": 0,
  "contrast": 100,
  "saturation": 100,
  "middle_gray": 50,
  "peak_luminance": 650,
  "debanding": true,
  "max_streams": 1,
  "output_buffers": 3
}
```

Select it in the existing video provider's trusted startup settings:

```json
{
  "gpu_processor_service": "amaterasu.rtx-hdr/1",
  "require_frame_lease": true
}
```

These are settings fragments, not complete signed host profiles. Package the
shared library as an ordinary independently signed module, allow it to provide
`amaterasu.rtx-hdr/1`, and add it to video's signed startup dependencies and
`native_use` grants. Keep the existing capture and encoder grants/dependencies.
Do not combine this setting with `cpu_processor_service`. Installing a module
without those composition changes does not select it.

Use an SDR capture source providing immutable DMA-BUF leases (capture v1.6 or
the supported negotiated DMA-BUF resource adapter), and an encoder with v1.2
lease submission supporting packed HDR RGB input. The stream must negotiate
10-bit HDR HEVC or AV1. The source display remains SDR; the processed output
and client stream are HDR. A capture that already supplies HDR is rejected,
preventing double conversion. The current automatic display configuration must
be composed accordingly; this plugin does not toggle desktop HDR.

## Pixels and ownership

Input is same-resolution, single-plane `XR24`, `AR24`, `XB24` or `AB24`, tagged
SDR sRGB. EGL samples its channel ordering into GPU RGBA8 for NVIDIA. Output is
`AB30` (R10G10B10A2), BT.2020/PQ, with source sequence, timestamp and freshness
preserved. Each dimension is 16..8192 and a packed frame is at most 128 MiB.
There is no resize, CPU staging, software tone-map fallback, or HDR-input bypass.

Each stream owns persistent GPU images, a TrueHDR effect, a CUDA stream and
1..4 output buffers. An output stays immutable until its last retained reference
and completion fence retire. A full pool returns `MEDIA_BUSY`. The provider
finishes every borrowed input read and output write before delivery; no frame
pointer or asynchronous callback escapes. Acquire and completion sync_file
fences are validated and waited with a one-second deadline. Failed completion
cannot permit buffer reuse; an unprovable teardown aborts the worker rather
than returning a false retirement barrier.

The default maximum is one active stream per binding (configurable to four),
and each output permits at most 64 references. Cancelled identities cannot be
reopened. Retirement history is bounded at 1024 identities, after which new
streams require a new binding. Cancelling a stream requires downstream output
references to have retired; video already cancels encoders before processors.
Factory bindings isolate consumer state. CUDA and EGL contexts are restored
after calls. Vendor libraries remain mapped for their process-wide callbacks;
effect allocations, surfaces, fds and streams retire with their owners.

Processor v1.1 supplies output HDR10 metadata through the SDK. This prevents
video from advertising the original SDR source's metadata for converted pixels.
The mastering luminance is the configured target; unknown measured content
light levels remain zero. Video checks the output dynamic range before invoking
the effect and fails an incompatible stream without skipping processing.

## Validation and remaining parity

Normal tests cover tuning bounds, unsupported inputs, SDK provider replacement,
retention, sink rejection, metadata version/size checks and stream-range rejection.
The opt-in test uses real GBM/EGL source and output surfaces, real NVIDIA
inference, acquire/completion fences and an exhausted output pool:

```sh
AMATERASU_TRUEHDR_RUNTIME=/opt/amaterasu/nvidia-vfx-0.2.0.0 \
  cargo test -p amaterasu-rtx-hdr --features linux-nvidia \
  nvidia_gpu_processes_real_pixels_and_preserves_bounded_ownership -- --ignored --nocapture
```

It compares every exported ten-bit RGB code against NVIDIA's GPU output, checks
deterministic repeated frames and confirms saturation changes actual pixels.
Readback exists only in the test harness. This was exercised on an RTX 4090 with
driver 615.71.09. It does not establish installed-host, encoded-stream, client
display brightness, or Windows/NVIDIA App pixel parity.

Vibeshine was used as a behavioral reference. Windows resource integration,
per-application/foreground policy, live control changes, Vibeshine's additional
peak-luminance correction/calibration and fullscreen detection remain separate
parity work. Controls here map directly to the NVIDIA VFX parameters, without
claiming parity with Vibeshine's corrected tuning. This change does not install
or activate the plugin in an existing host profile.

References: [NVIDIA TrueHDR filter](https://docs.nvidia.com/maxine/vfx/latest/Filters/TrueHDR.html),
[Processing SDK](../../docs/gpu-processing-sdk.md).
