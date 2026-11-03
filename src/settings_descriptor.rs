//! Settings exposed by the optional RTX HDR provider.
use amaterasu_configuration_schema::{Definition, Fields, Values};
use amaterasu_module_sdk::extension_api::{
    ApplyPolicy, ExtensionDescriptor, MethodRef, OptionDescriptor, SettingScope,
};

pub const PROVIDER: &str = "amaterasu.rtx-hdr";
pub const SERVICE: &str = "nonary.rtx-hdr/1";

fn integer(default: i64, minimum: i64, maximum: i64) -> Definition {
    Definition::Integer {
        default,
        minimum: Some(minimum),
        maximum: Some(maximum),
    }
}

pub fn definitions() -> Fields {
    [
        ("rtx_hdr_enable", Definition::Boolean { default: false }),
        ("rtx_hdr_peak_brightness", integer(1000, 400, 2000)),
        ("rtx_hdr_middle_gray", integer(50, 10, 100)),
        ("rtx_hdr_contrast", integer(0, -100, 100)),
        ("rtx_hdr_saturation", integer(0, -100, 100)),
        ("rtx_hdr_debanding", Definition::Boolean { default: true }),
    ]
    .into_iter()
    .map(|(name, definition)| (name.into(), definition))
    .collect()
}

pub fn validate(values: &Values) -> Result<(), String> {
    let fields = definitions();
    if values.len() != fields.len() || fields.keys().any(|name| !values.contains_key(name)) {
        return Err("incomplete_settings".into());
    }
    amaterasu_configuration_schema::validate_values(&fields, values)
}

pub fn descriptor() -> ExtensionDescriptor {
    let fields = definitions();
    let descriptions = [
        (
            "rtx_hdr_enable",
            "Enable RTX HDR",
            "Convert SDR video to HDR on a supported NVIDIA GPU and HDR client. Start a new application session after changing this setting.",
        ),
        (
            "rtx_hdr_peak_brightness",
            "Peak brightness (nits)",
            "Set the target HDR peak brightness.",
        ),
        (
            "rtx_hdr_middle_gray",
            "Middle gray",
            "Adjust the brightness of midtones.",
        ),
        (
            "rtx_hdr_contrast",
            "Contrast",
            "Adjust the contrast of the HDR conversion.",
        ),
        (
            "rtx_hdr_saturation",
            "Saturation",
            "Adjust color saturation of the HDR conversion.",
        ),
        (
            "rtx_hdr_debanding",
            "Debanding",
            "Reduce visible bands in gradients.",
        ),
    ];
    let options = descriptions
        .into_iter()
        .map(|(id, label, help)| OptionDescriptor {
            id: id.into(),
            label: label.into(),
            help: help.into(),
            definition: fields[id].clone(),
            presentation: None,
            allowed_scopes: if id == "rtx_hdr_enable" {
                // Enablement selects the source range during session prepare;
                // a live session override cannot be applied to that session and
                // its identity is not reused by the next session.
                vec![
                    SettingScope::Host,
                    SettingScope::Application,
                    SettingScope::PairedClient,
                ]
            } else {
                vec![
                    SettingScope::Host,
                    SettingScope::Application,
                    SettingScope::PairedClient,
                    SettingScope::Session,
                ]
            },
            apply_policy: if id == "rtx_hdr_enable" {
                ApplyPolicy::NextSession
            } else {
                ApplyPolicy::NextStream
            },
            required_capabilities: vec![],
            choice_labels: Default::default(),
        })
        .collect();
    ExtensionDescriptor {
        provider: PROVIDER.into(),
        display_name: Some("RTX HDR".into()),
        revision: 1,
        options,
        actions: vec![],
        capabilities: vec![],
        settings_validate: MethodRef {
            service: SERVICE.into(),
            method: "settings_validate".into(),
            interface_major: 1,
            interface_minor: 0,
            schema_revision: 1,
        },
    }
}

/// Metadata is generated from the same descriptor and API registrations that
/// the plugin exposes at runtime. Packaging appends lifecycle registrations.
pub fn installation_metadata() -> serde_json::Value {
    let methods: Vec<_> = std::iter::once(crate::extension::settings_registration())
        .chain(crate::lifecycle::registrations())
        .chain(std::iter::once(crate::lifecycle::intent_registration()))
        .map(|command| match command {
            amaterasu_module_sdk::Command::RegisterApi { method } => method,
            _ => unreachable!("only typed API registrations are metadata"),
        })
        .collect();
    serde_json::json!({"descriptor": descriptor(), "methods": methods})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptor_defaults_and_scoped_ranges_validate() {
        let descriptor = descriptor();
        amaterasu_module_sdk::extension_api::validate_descriptor(&descriptor).unwrap();
        let enable = descriptor
            .options
            .iter()
            .find(|option| option.id == "rtx_hdr_enable")
            .unwrap();
        assert_eq!(enable.apply_policy, ApplyPolicy::NextSession);
        assert!(!enable.allowed_scopes.contains(&SettingScope::Session));
        assert!(enable.allowed_scopes.contains(&SettingScope::PairedClient));
        let mut values = amaterasu_configuration_schema::defaults(&definitions()).unwrap();
        assert_eq!(values["rtx_hdr_enable"], false);
        assert!(validate(&values).is_ok());
        values.insert("rtx_hdr_peak_brightness".into(), json!(2001));
        assert!(validate(&values).is_err());
        values.insert("rtx_hdr_peak_brightness".into(), json!(1000));
        values.insert("runtime_directory".into(), json!("/attacker"));
        assert!(validate(&values).is_err());
        values.remove("runtime_directory");
        values.remove("rtx_hdr_contrast");
        assert!(validate(&values).is_err());
    }
}
