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

Create a local SDK workspace from an Amaterasu checkout, clone this repository
into it, and build the plugin independently:

```sh
python3 /path/to/amaterasu/scripts/create-processing-sdk.py /absolute/path/to/sdk-workspace
git clone https://github.com/nonary-streams/amaterasu-rtx-hdr.git /absolute/path/to/sdk-workspace/modules/rtx-hdr
cd /absolute/path/to/sdk-workspace
cargo build -p amaterasu-rtx-hdr --features linux-nvidia
```

CUDA, EGL, OpenGL, GBM and DRM development headers are required. The SDK
workspace includes the C headers and contract crates. The NVIDIA runtime helper
is a separate Rust workspace and is not a dependency of the generic SDK.
Vendor libraries remain separate from the plugin package.

The host selects the provider through the frame-processing SDK. The plugin does
not select capture, video, an encoder, frame limiter, or a virtual display. Its
[`examples/component-linux.json`](examples/component-linux.json) describes the
independently built provider and depends only on the host runtime. A signed
composition must bind a compatible frame-processing consumer to
`amaterasu.rtx-hdr/1`, grant native use, and keep the provider loaded until the
consumer retires. Capture and encoder resource compatibility is negotiated by
the SDK; a particular capture plugin is not required.

The conversion requires supported SDR input pixels and an HDR output stream.
Both eight-bit and ten-bit SDR input are supported when the selected capture
provider supplies a compatible resource and color description. The frame
processor publishes its supported output resources and metadata for the selected
encoder to negotiate. Incompatible resource, format or synchronization choices
fail explicitly. An SDR output stream bypasses conversion.

Trusted startup composition can select the Display Settings SDK when the source
display needs an SDR adjustment. RTX HDR acquires a scoped HDR-off and
wide-color-gamut-off lease from the selected implementation; the source owner
closes the scope and the provider restores the display. The output stream remains
HDR. A composition with an already suitable SDR source may omit that capability.
Foreground/fullscreen restrictions likewise use an explicitly selected
foreground-context implementation and do not require one when those restrictions
are off. No optional implementation is silently selected by the plugin.

For the standard Linux host, a configuration with no optional display or
foreground provider is initialized as follows. Replace paths with trusted local
paths:

```sh
amaterasu-release init-linux-session ROOT --external-extension-composition \
  --rtx-hdr-runtime ROOT/data/state/rtx-hdr/runtime
amaterasu-release pack-linux-session-rtx-hdr ROOT ARTIFACTS PACKAGE SEQUENCE
```

The packer omits the profile's foreground, display-settings, and frame-limiter
components when the trusted policy does not select them. It rejects omission of
a provider still selected by policy. This prepares a complete signed set; package
activation and the installed service's publisher and policy checks remain
separate.

A native-only selection is provided in
[`examples/frame-only-selection.json`](examples/frame-only-selection.json).
It explicitly supplies stream settings and selects no lifecycle, registry,
display, or foreground capability. It requires a compatible SDR source and HDR
output. For a managed session, supply a complete `ProcessingSelection` with the
chosen stream, lifecycle and registry bindings and the appropriate grants; each
binding specifies its principal, service alias, contract identity and version.

Use `prepare-rtx-hdr-bootstrap ROOT SHA256 OUTPUT --processing-selection
SELECTION_JSON` to create a review-only candidate. For production, prepare and
approve the corresponding `review-package-activation-policy` report before
activation. Sign the package and a local repository using the existing approved
publisher as described in [repository publishing](../../docs/rtx-hdr-repository-publishing.md).
Then follow [local signed package updates](../../docs/local-package-updates.md)
to import and activate the exact verified digest. Those instructions retain the
existing publisher, policy, replay and service-pin checks.

An installation whose policy still holds the earlier RTX HDR wiring in provider
settings needs no separate migration command. `review-package-activation-policy`
recognizes it, proposes the destination with that wiring moved into a processing
composition, and records `legacy_rtx_wiring_converted` in the report. Capture,
frame-lease, runtime and other host settings are carried over unchanged. Because
the older host cannot parse a composition policy, activate through the matching
new host, and `provision-package-activation` replaces an earlier allowlist only
when the same publisher issued it and every value it approved is already applied.

The separate CPU pass-through example in the SDK workspace demonstrates the
same native-only flow. See [the frame-processing SDK guide](../../docs/gpu-processing-sdk.md)
for its complete component, selection, review and signing example.

## Pixels and ownership

Input can be packed SDR DMA-BUF (`XR24`, `AR24`, `XB24`, `AB24`, `XR30`,
`AR30`, `XB30`, or `AB30`) or a supported CPU BGRA8, NV12, or P010 SDR resource.
The provider owns any CPU upload and format conversion. Output is negotiated CUDA
R10G10B10A2 in BT.2020/PQ, with source sequence, timestamp and freshness
preserved. Each dimension is 16..8192. The effect does not resize or accept HDR
input for conversion.

Each stream owns persistent GPU images, a TrueHDR effect, a CUDA stream and
1..4 output buffers. An output remains immutable until its retained references
and completion dependencies retire. A full pool returns `MEDIA_BUSY`. The
provider retains input resources until its queued GPU reads finish; resources
without compatible ownership or synchronization are rejected. Failed completion
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

The frame-processing SDK negotiates the CUDA output and readiness event with
the selected consumer. NVENC can convert that output to P010 on an ordered CUDA
stream. Successful first frames and conversion enable/disable transitions emit
`rtx_hdr_conversion` with the actual conversion state and output color space.
This establishes host processing, not client HDR presentation.

The processor supplies output HDR10 metadata through the SDK, independently of
the source metadata. The mastering luminance is the fixed 2000-nit supported
ceiling; unknown measured content light levels remain zero. Video checks the
output dynamic range before invoking the effect.

## Foreground policy

RTX HDR defaults to per-application scope. Opt in from an application's RTX
HDR settings, or select global scope and enable RTX HDR in the host settings.
These choices apply to the next stream. For HDR output, RTX HDR uses an SDR
capture source and converts it to HDR. An SDR output request bypasses RTX HDR
for that stream without changing the saved settings.
Unselected streams do not route through the RTX processor.

`rtx_hdr_foreground_only` and `rtx_hdr_fullscreen_only` default to false. When
foreground matching is enabled, the plugin applies
TrueHDR only while the selected foreground provider reports the exact executable
associated with the stream lifecycle target. Missing, stale, or mismatched
observations use the SDR-in-HDR fallback. When fullscreen-only is enabled,
conversion additionally requires a fresh, explicitly fullscreen observation;
unknown fullscreen state also falls back. Trusted startup composition must bind
a foreground provider through the `foreground` field of its
`ProcessingSelection` and grant this plugin access to that versioned
foreground-context implementation and to lifecycle `read_provider_context`.
`review-package-activation-policy` binds the built-in provider by default when
the installed policy approves that component. No observer is silently selected.
Foreground identity
is an absolute executable path; launchers, Steam URIs and Proton/Wine processes
may not match a game's configured executable, so these cases fall back rather
than establishing game identity. Native Wayland requires a compositor-backed
provider; an X11 observer cannot see all native Wayland windows. The foreground
policy has not been qualified in an installed host or live gameplay.

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

Vibeshine was used as a behavioral reference. Windows resource integration
and live fullscreen detection qualification remain separate parity work. The live controls and peak
correction are independently implemented; numerical equivalence with Vibeshine
has not been measured. This change does not install or activate the plugin in an
existing host profile.

References: [NVIDIA TrueHDR filter](https://docs.nvidia.com/maxine/vfx/latest/Filters/TrueHDR.html),
[Processing SDK](../../docs/gpu-processing-sdk.md).

## Application settings UI

`ui/application.js` is the plugin-owned application editor UI. It registers an
application section, renders the RTX HDR controls through the shared browser UI
SDK, and saves through the application-scoped settings helper. Slider edits are
debounced and saved serially; changes made during a pending save remain queued.
Inheritance removes only the app-specific value. Enablement and tuning use the
live-settings contract. HDR streams using this provider reserve the
SDR-to-HDR processing path even when conversion starts disabled: turning the
effect off uses SDR fallback in the same HDR output, and turning it back on
restores conversion without restarting capture or encoding. Ordinary SDR
streams keep their negotiated format; changing the output format still needs
a new stream.

Package this separately signed UI with `ui/application-ui.json`. Copy only
`application.js` into the publisher's source-assets directory, then run:

```text
amaterasu-web-extension pack SOURCE OUTPUT rtx-hdr "RTX HDR" application.js SEQUENCE EXPIRES_UNIX SECRET_HEX_FILE ui/application-ui.json
```

The configuration declares `placements: ["application"]` and
`settings_provider: "amaterasu.rtx-hdr"`. The host supplies the section and fixed
application scope; it does not contain the RTX HDR form implementation. Selecting
this signed UI replaces the host's generic settings form for this provider.
Other plugins may attach application UI without declaring a settings provider.
The existing settings registry validates values and permissions; the UI helper
does not grant access. Building the native module alone does not select this
separately approved browser UI.

## Building this repository

This module lives in its own repository and is mounted by
[Amaterasu](https://github.com/nonary-streams/amaterasu) at `modules/rtx-hdr`,
where it builds as a member of the host workspace. Its manifest inherits shared
dependencies and lints from that workspace, so a standalone build stages the
same layout around a checkout of the
[SDK](https://github.com/nonary-streams/amaterasu-sdk) at the commit named in
`sdk-revision`:

```sh
git clone https://github.com/nonary-streams/amaterasu-sdk .sdk
git -C .sdk checkout "$(cat sdk-revision)"
export CARGO_TARGET_DIR="$PWD/target"
python3 scripts/build-native.py .sdk --fmt
python3 scripts/build-native.py .sdk --test
python3 scripts/build-native.py .sdk --clippy
python3 scripts/build-native.py .sdk --release --artifacts dist
```

Neither checkout is modified. `Cargo.lock` records the resolution those
commands were validated with; after changing `sdk-revision` or a dependency,
refresh it with `python3 scripts/build-native.py .sdk --update-lock`.

Continuous integration runs the same four commands on Linux, Windows and
macOS. Pushing a tag `vX.Y.Z` that matches the version in `Cargo.toml` builds
the Linux and Windows libraries, attests where they were built, and publishes
them as a GitHub release. This crate takes its version from the workspace it
builds in, so the tag has to match the SDK's workspace version. A host installs this module as part of the signed
Amaterasu module set; a release here is not an installable package on its own.
