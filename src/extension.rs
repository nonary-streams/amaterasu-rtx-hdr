//! Typed settings validation for the explicitly selected extension registry.
use amaterasu_module_sdk::{
    ApiFieldKind, ApiMethod, ApiPrivilege, ApiSchema, Command, ModuleResult,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) fn registry_principal(
    binding: Option<&amaterasu_module_sdk::processing_composition::CapabilityBinding>,
) -> ModuleResult<Option<String>> {
    let Some(binding) = binding else {
        return Ok(None);
    };
    amaterasu_module_sdk::processing_composition::require_contract(
        binding,
        amaterasu_module_sdk::extension_api::EXTENSION_REGISTRY_SERVICE,
        1,
        0,
    )?;
    Ok(Some(binding.principal.clone()))
}

pub(crate) fn settings_registration() -> Command {
    Command::RegisterApi {
        method: ApiMethod {
            service: crate::settings_descriptor::SERVICE.into(),
            method: "settings_validate".into(),
            interface_major: 1,
            interface_minor: 0,
            request: ApiSchema {
                required: BTreeMap::from([("values".into(), ApiFieldKind::Object)]),
                optional: BTreeMap::new(),
                allow_extra: false,
            },
            response: ApiSchema {
                required: BTreeMap::from([
                    ("ok".into(), ApiFieldKind::Boolean),
                    ("valid".into(), ApiFieldKind::Boolean),
                ]),
                optional: BTreeMap::from([("message".into(), ApiFieldKind::String)]),
                allow_extra: false,
            },
            privilege: ApiPrivilege::User,
        },
    }
}

pub(crate) fn handle_settings(
    caller: &str,
    selected_registry: Option<&str>,
    has_foreground: bool,
    payload: Value,
) -> Value {
    if selected_registry != Some(caller) {
        return json!({"ok":true,"valid":false,"message":"unauthorized caller"});
    }
    let Some(envelope) = payload.as_object().filter(|object| object.len() == 4) else {
        return json!({"ok":true,"valid":false,"message":"invalid request"});
    };
    if envelope.get("method") != Some(&json!("settings_validate"))
        || envelope.get("interface_major") != Some(&json!(1))
        || envelope.get("interface_minor") != Some(&json!(0))
    {
        return json!({"ok":true,"valid":false,"message":"invalid request"});
    }
    let Some(values) = envelope
        .get("arguments")
        .and_then(Value::as_object)
        .filter(|arguments| arguments.len() == 1)
        .and_then(|arguments| arguments.get("values"))
        .and_then(|values| serde_json::from_value(values.clone()).ok())
    else {
        return json!({"ok":true,"valid":false,"message":"invalid settings"});
    };
    match crate::settings_descriptor::validate(&values) {
        Ok(())
            if !has_foreground
                && ["rtx_hdr_foreground_only", "rtx_hdr_fullscreen_only"]
                    .iter()
                    .any(|name| values.get(*name) == Some(&json!(true))) =>
        {
            json!({"ok":true,"valid":false,"message":"foreground provider required"})
        }
        Ok(()) => json!({"ok":true,"valid":true}),
        Err(_) => json!({"ok":true,"valid":false,"message":"invalid settings"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restricted_settings_require_a_selected_foreground_provider() {
        let mut values =
            amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
                .unwrap();
        for name in ["rtx_hdr_foreground_only", "rtx_hdr_fullscreen_only"] {
            values.insert(name.into(), json!(true));
            let request = json!({"method":"settings_validate","interface_major":1,
                "interface_minor":0,"arguments":{"values":values}});
            assert_eq!(
                handle_settings("registry", Some("registry"), false, request.clone())["valid"],
                false
            );
            assert_eq!(
                handle_settings("registry", Some("registry"), true, request)["valid"],
                true
            );
            values.insert(name.into(), json!(false));
        }
    }

    #[test]
    fn only_selected_registry_can_validate_complete_settings() {
        let values =
            amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
                .unwrap();
        let request = json!({"method":"settings_validate","interface_major":1,"interface_minor":0,"arguments":{"values":values}});
        assert_eq!(
            handle_settings("registry", Some("registry"), true, request.clone())["valid"],
            true
        );
        assert_eq!(
            handle_settings("other", Some("registry"), true, request.clone())["valid"],
            false
        );
        assert_eq!(
            handle_settings("registry", None, true, request)["valid"],
            false
        );
    }
}
