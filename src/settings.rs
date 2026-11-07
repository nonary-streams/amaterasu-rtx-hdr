use amaterasu_media::HdrMetadata;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Trusted startup settings. Runtime location is an administrator composition
/// choice; a frame, remote request, or client cannot select vendor libraries.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub runtime_directory: PathBuf,
    /// Require the host-installed, hash-verified NVIDIA runtime inventory.
    #[serde(default)]
    pub require_verified_runtime: bool,
    /// Per-stream conversion switch. False retains the HDR-compatible SDR path.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub device: u32,
    #[serde(default = "default_contrast")]
    pub contrast: u32,
    #[serde(default = "default_contrast")]
    pub saturation: u32,
    #[serde(default = "default_middle_gray")]
    pub middle_gray: u32,
    #[serde(default = "default_luminance")]
    pub peak_luminance: u32,
    #[serde(default = "default_debanding")]
    pub debanding: bool,
    /// SDR fallback white level adjustment, in the inclusive range 0..=100.
    #[serde(default)]
    pub sdr_brightness: u32,
    /// Gate conversion on the selected application being in the foreground.
    #[serde(default)]
    pub foreground_only: bool,
    /// Gate conversion on the active window being fullscreen.
    #[serde(default)]
    pub fullscreen_only: bool,
    #[serde(default = "default_streams")]
    pub max_streams: usize,
    #[serde(default = "default_buffers")]
    pub output_buffers: usize,
}
fn default_enabled() -> bool {
    true
}
fn default_contrast() -> u32 {
    100
}
fn default_middle_gray() -> u32 {
    50
}
fn default_luminance() -> u32 {
    650
}
fn default_debanding() -> bool {
    true
}
fn default_streams() -> usize {
    1
}
fn default_buffers() -> usize {
    3
}

impl Settings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.runtime_directory.is_absolute() {
            return Err("runtime_directory must be absolute");
        }
        if self.device > 15
            || self.contrast > 200
            || self.saturation > 200
            || self.sdr_brightness > 100
            || !(10..=100).contains(&self.middle_gray)
            || !(400..=2000).contains(&self.peak_luminance)
            || !(1..=4).contains(&self.max_streams)
            || !(1..=4).contains(&self.output_buffers)
        {
            return Err("RTX HDR settings are outside their supported bounds");
        }
        Ok(())
    }

    #[cfg(any(test, all(target_os = "linux", feature = "linux-nvidia")))]
    pub(crate) fn parameters(&self) -> [u32; 5] {
        [
            self.contrast,
            self.saturation,
            self.middle_gray,
            self.peak_luminance,
            u32::from(!self.debanding),
        ]
    }

    pub fn hdr_metadata(&self) -> HdrMetadata {
        // BT.2020/D65 mastering primaries, in the SDK's documented RGB order.
        // Advertise the maximum supported mastering ceiling for the stream so
        // live target changes never outrun the already-negotiated metadata.
        // Content light levels are unknown; do not invent measured MaxCLL/FALL.
        HdrMetadata {
            enabled: 1,
            fields: [
                35400, 14600, 8500, 39850, 6550, 2300, 15635, 16450, 2000, 0, 0, 0,
            ],
            reserved: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_use_vendor_defaults_and_reject_invalid_controls() {
        let settings: Settings =
            serde_json::from_value(serde_json::json!({"runtime_directory":"/runtime"})).unwrap();
        assert_eq!(settings.parameters(), [100, 100, 50, 650, 0]);
        assert!(settings.validate().is_ok());
        for (field, value) in [
            ("contrast", 201),
            ("saturation", 201),
            ("sdr_brightness", 101),
            ("middle_gray", 9),
            ("peak_luminance", 2001),
            ("max_streams", 0),
            ("output_buffers", 5),
            ("device", 16),
        ] {
            let mut config = serde_json::to_value(&settings).unwrap();
            config[field] = value.into();
            assert!(
                serde_json::from_value::<Settings>(config)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{field}"
            );
        }
        let mut relative = settings.clone();
        relative.runtime_directory = "relative".into();
        assert!(relative.validate().is_err());
        assert_eq!(settings.hdr_metadata().fields[8], 2000);
        let mut brighter = settings.clone();
        brighter.peak_luminance = 2000;
        assert_eq!(
            brighter.hdr_metadata().fields,
            settings.hdr_metadata().fields
        );
        assert_eq!(&settings.hdr_metadata().fields[10..], &[0, 0]);
    }
}

/// Provider-owned controls for a single stream. Conversion and tuning may be
/// updated while the HDR-compatible stream is active. Runtime path
/// and GPU allocation limits remain trusted startup configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSettings {
    pub rtx_hdr_enable: bool,
    #[serde(default = "legacy_global_scope")]
    pub rtx_hdr_scope: String,
    #[serde(default)]
    pub rtx_hdr_application_enable: bool,
    pub rtx_hdr_peak_brightness: u32,
    pub rtx_hdr_middle_gray: u32,
    pub rtx_hdr_contrast: i32,
    pub rtx_hdr_saturation: i32,
    pub rtx_hdr_debanding: bool,
    #[serde(default)]
    pub rtx_hdr_sdr_brightness: u32,
    #[serde(default)]
    pub rtx_hdr_foreground_only: bool,
    #[serde(default)]
    pub rtx_hdr_fullscreen_only: bool,
    /// Admitted target used by this provider to resolve foreground policy.
    #[serde(default)]
    pub foreground_target: Option<amaterasu_module_sdk::extension_api::LifecycleTarget>,
}

fn legacy_global_scope() -> String {
    "global".into()
}

impl StreamSettings {
    pub fn apply(&self, startup: &Settings) -> Result<Settings, &'static str> {
        if !(-100..=100).contains(&self.rtx_hdr_contrast)
            || !(-100..=100).contains(&self.rtx_hdr_saturation)
        {
            return Err("invalid RTX HDR stream settings");
        }
        let mut configured = startup.clone();
        configured.enabled = match self.rtx_hdr_scope.as_str() {
            "global" => self.rtx_hdr_enable,
            "application" => self.rtx_hdr_application_enable,
            _ => return Err("invalid RTX HDR scope"),
        };
        configured.contrast = (self.rtx_hdr_contrast + 100) as u32;
        configured.saturation = (self.rtx_hdr_saturation + 100) as u32;
        configured.middle_gray = self.rtx_hdr_middle_gray;
        configured.peak_luminance = self.rtx_hdr_peak_brightness;
        configured.debanding = self.rtx_hdr_debanding;
        configured.sdr_brightness = self.rtx_hdr_sdr_brightness;
        configured.foreground_only = self.rtx_hdr_foreground_only;
        configured.fullscreen_only = self.rtx_hdr_fullscreen_only;
        configured.validate()?;
        Ok(configured)
    }
}

#[cfg(test)]
mod stream_settings_tests {
    use super::*;

    #[test]
    fn conversion_scope_selects_global_or_application_enablement() {
        let startup: Settings =
            serde_json::from_value(serde_json::json!({"runtime_directory":"/trusted"})).unwrap();
        let mut values =
            amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
                .unwrap();
        values.insert("rtx_hdr_enable".into(), true.into());
        let mut controls: StreamSettings =
            serde_json::from_value(serde_json::json!(values)).unwrap();
        assert!(!controls.apply(&startup).unwrap().enabled);
        controls.rtx_hdr_application_enable = true;
        assert!(controls.apply(&startup).unwrap().enabled);
        controls.rtx_hdr_scope = "global".into();
        controls.rtx_hdr_application_enable = false;
        assert!(controls.apply(&startup).unwrap().enabled);
        controls.rtx_hdr_enable = false;
        assert!(!controls.apply(&startup).unwrap().enabled);
    }

    #[test]
    fn stream_controls_cannot_change_trusted_runtime_and_are_bounded() {
        let startup: Settings =
            serde_json::from_value(serde_json::json!({"runtime_directory":"/trusted"})).unwrap();
        let mut controls: StreamSettings = serde_json::from_value(serde_json::json!({
            "rtx_hdr_enable": true, "rtx_hdr_peak_brightness": 1000,
            "rtx_hdr_middle_gray": 50, "rtx_hdr_contrast": -100,
            "rtx_hdr_saturation": 100, "rtx_hdr_debanding": true
        }))
        .unwrap();
        let selected = controls.apply(&startup).unwrap();
        assert_eq!(selected.runtime_directory, startup.runtime_directory);
        assert_eq!(selected.parameters(), [0, 200, 50, 1000, 0]);
        assert!(!selected.foreground_only);
        assert!(!selected.fullscreen_only);
        assert!(controls.foreground_target.is_none());
        controls.rtx_hdr_foreground_only = true;
        assert!(controls.apply(&startup).unwrap().foreground_only);
        controls.rtx_hdr_fullscreen_only = true;
        assert!(controls.apply(&startup).unwrap().fullscreen_only);
        let target = serde_json::json!({"kind":"session", "host_epoch":"host", "session_id":"session", "generation":1});
        let mut with_target = serde_json::to_value(&controls).unwrap();
        with_target["foreground_target"] = target;
        assert!(
            serde_json::from_value::<StreamSettings>(with_target)
                .unwrap()
                .foreground_target
                .is_some()
        );
        controls.rtx_hdr_contrast = 101;
        assert!(controls.apply(&startup).is_err());
        controls.rtx_hdr_contrast = 0;
        controls.rtx_hdr_sdr_brightness = 101;
        assert!(controls.apply(&startup).is_err());
        controls.rtx_hdr_sdr_brightness = 100;
        assert_eq!(controls.apply(&startup).unwrap().sdr_brightness, 100);
        controls.rtx_hdr_enable = false;
        assert!(!controls.apply(&startup).unwrap().enabled);
        assert!(
            serde_json::from_value::<StreamSettings>(
                serde_json::json!({"runtime_directory":"/untrusted"})
            )
            .is_err()
        );
    }
}
