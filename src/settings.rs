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
    #[serde(default = "default_streams")]
    pub max_streams: usize,
    #[serde(default = "default_buffers")]
    pub output_buffers: usize,
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
        // Content light levels are unknown; do not invent measured MaxCLL/FALL.
        HdrMetadata {
            enabled: 1,
            fields: [
                35400,
                14600,
                8500,
                39850,
                6550,
                2300,
                15635,
                16450,
                self.peak_luminance as u16,
                0,
                0,
                0,
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
        assert_eq!(settings.hdr_metadata().fields[8], 650);
        assert_eq!(&settings.hdr_metadata().fields[10..], &[0, 0]);
    }
}

/// Frozen, provider-owned controls for a single stream. The runtime path and
/// GPU allocation limits remain trusted startup configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSettings {
    pub rtx_hdr_enable: bool,
    pub rtx_hdr_peak_brightness: u32,
    pub rtx_hdr_middle_gray: u32,
    pub rtx_hdr_contrast: i32,
    pub rtx_hdr_saturation: i32,
    pub rtx_hdr_debanding: bool,
}

impl StreamSettings {
    pub fn apply(&self, startup: &Settings) -> Result<Settings, &'static str> {
        if !self.rtx_hdr_enable
            || !(-100..=100).contains(&self.rtx_hdr_contrast)
            || !(-100..=100).contains(&self.rtx_hdr_saturation)
        {
            return Err("invalid RTX HDR stream settings");
        }
        let mut configured = startup.clone();
        configured.contrast = (self.rtx_hdr_contrast + 100) as u32;
        configured.saturation = (self.rtx_hdr_saturation + 100) as u32;
        configured.middle_gray = self.rtx_hdr_middle_gray;
        configured.peak_luminance = self.rtx_hdr_peak_brightness;
        configured.debanding = self.rtx_hdr_debanding;
        configured.validate()?;
        Ok(configured)
    }
}

#[cfg(test)]
mod stream_settings_tests {
    use super::*;

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
        controls.rtx_hdr_contrast = 101;
        assert!(controls.apply(&startup).is_err());
        controls.rtx_hdr_contrast = 0;
        controls.rtx_hdr_enable = false;
        assert!(controls.apply(&startup).is_err());
        assert!(
            serde_json::from_value::<StreamSettings>(
                serde_json::json!({"runtime_directory":"/untrusted"})
            )
            .is_err()
        );
    }
}
