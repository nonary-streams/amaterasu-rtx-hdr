//! Settings exposed by the optional RTX HDR provider.
use amaterasu_configuration_schema::{Definition, Fields, Values};
use amaterasu_module_sdk::extension_api::{
    ApplyPolicy, ExtensionDescriptor, MethodRef, OptionDescriptor, OptionPresentation, SettingScope,
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
        (
            "rtx_hdr_scope",
            Definition::Enum {
                default: "application".into(),
                options: vec!["application".into(), "global".into()],
            },
        ),
        (
            "rtx_hdr_application_enable",
            Definition::Boolean { default: false },
        ),
        ("rtx_hdr_peak_brightness", integer(1000, 400, 2000)),
        ("rtx_hdr_middle_gray", integer(50, 10, 100)),
        ("rtx_hdr_contrast", integer(0, -100, 100)),
        ("rtx_hdr_saturation", integer(0, -100, 100)),
        ("rtx_hdr_debanding", Definition::Boolean { default: true }),
        ("rtx_hdr_sdr_brightness", integer(0, 0, 100)),
        (
            "rtx_hdr_fullscreen_only",
            Definition::Boolean { default: false },
        ),
        (
            "rtx_hdr_foreground_only",
            Definition::Boolean { default: false },
        ),
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
            "Enable RTX HDR in global mode.",
        ),
        (
            "rtx_hdr_scope",
            "Apply RTX HDR",
            "RTX HDR converts an SDR source to HDR output. SDR streams bypass the effect. Scope changes apply to the next stream.",
        ),
        (
            "rtx_hdr_application_enable",
            "Enable RTX HDR for this application",
            "Opt this application in.",
        ),
        (
            "rtx_hdr_peak_brightness",
            "Peak brightness (nits)",
            "Set the target HDR peak brightness.",
        ),
        (
            "rtx_hdr_middle_gray",
            "Middle gray",
            "Adjust midtone brightness.",
        ),
        ("rtx_hdr_contrast", "Contrast", "Adjust contrast."),
        (
            "rtx_hdr_saturation",
            "Saturation",
            "Adjust color saturation.",
        ),
        (
            "rtx_hdr_fullscreen_only",
            "Only convert fullscreen video",
            "Use SDR fallback while fullscreen state is false or unavailable.",
        ),
        (
            "rtx_hdr_foreground_only",
            "Only convert the foreground application",
            "Use SDR fallback when the application is not confirmed foreground.",
        ),
        (
            "rtx_hdr_sdr_brightness",
            "SDR fallback brightness",
            "Adjust fallback brightness.",
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
            presentation: match fields[id] {
                Definition::Integer { .. } => Some(OptionPresentation::Slider),
                Definition::Enum { .. } => Some(OptionPresentation::RadioGroup),
                _ => None,
            },
            allowed_scopes: if id == "rtx_hdr_scope" {
                vec![SettingScope::Host]
            } else if id == "rtx_hdr_application_enable" {
                vec![SettingScope::Application]
            } else {
                vec![
                    SettingScope::Host,
                    SettingScope::Application,
                    SettingScope::PairedClient,
                    SettingScope::Session,
                ]
            },
            apply_policy: if matches!(id, "rtx_hdr_scope" | "rtx_hdr_application_enable") {
                ApplyPolicy::NextStream
            } else {
                ApplyPolicy::LiveAction
            },
            required_capabilities: vec![],
            choice_labels: if id == "rtx_hdr_scope" {
                [
                    ("application".into(), "Per application (opt in)".into()),
                    ("global".into(), "Globally".into()),
                ]
                .into()
            } else {
                Default::default()
            },
        })
        .collect();
    ExtensionDescriptor {
        provider: PROVIDER.into(),
        display_name: Some("RTX HDR".into()),
        default_expanded: true,
        revision: 3,
        options,
        actions: vec![],
        capabilities: vec![amaterasu_module_sdk::extension_api::STREAM_JOIN_CAPABILITY.into()],
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
    let old_fields: Fields = definitions()
        .into_iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "rtx_hdr_scope" | "rtx_hdr_application_enable"
            )
        })
        .collect();
    let old_allowed: std::collections::BTreeMap<_, _> = old_fields
        .keys()
        .map(|name| {
            (
                name.clone(),
                vec!["host", "application", "paired_client", "session"],
            )
        })
        .collect();
    let mut version_one_allowed = old_allowed.clone();
    version_one_allowed.insert(
        "rtx_hdr_enable".into(),
        vec!["host", "application", "paired_client"],
    );
    serde_json::json!({"descriptor": descriptor(), "methods": methods,
        "migrations":[{"from_schema_revision":1,"old_fields":old_fields,
            "old_allowed":version_one_allowed,"renames":{},"removed":[],"replacements":[]},
            {"from_schema_revision":2,"old_fields":old_fields,
            "old_allowed":old_allowed,"renames":{},"removed":[],"replacements":[]}]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptor_defaults_and_scoped_ranges_validate() {
        let descriptor = descriptor();
        assert!(descriptor.default_expanded);
        let metadata = installation_metadata();
        assert_eq!(metadata["descriptor"]["default_expanded"], true);
        amaterasu_module_sdk::extension_api::validate_descriptor(&descriptor).unwrap();
        let enable = descriptor
            .options
            .iter()
            .find(|option| option.id == "rtx_hdr_enable")
            .unwrap();
        assert_eq!(enable.apply_policy, ApplyPolicy::LiveAction);
        assert!(enable.allowed_scopes.contains(&SettingScope::Session));
        assert!(enable.allowed_scopes.contains(&SettingScope::PairedClient));
        for option in descriptor.options.iter().filter(|option| {
            !matches!(
                option.id.as_str(),
                "rtx_hdr_enable" | "rtx_hdr_scope" | "rtx_hdr_application_enable"
            )
        }) {
            assert_eq!(
                option.apply_policy,
                ApplyPolicy::LiveAction,
                "{}",
                option.id
            );
            if option.id != "rtx_hdr_debanding"
                && option.id != "rtx_hdr_foreground_only"
                && option.id != "rtx_hdr_fullscreen_only"
            {
                assert_eq!(
                    option.presentation,
                    Some(OptionPresentation::Slider),
                    "{}",
                    option.id
                );
            }
        }
        let mut values = amaterasu_configuration_schema::defaults(&definitions()).unwrap();
        assert_eq!(values["rtx_hdr_enable"], false);
        assert_eq!(values["rtx_hdr_scope"], "application");
        assert_eq!(values["rtx_hdr_application_enable"], false);
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
