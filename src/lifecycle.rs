//! Lifecycle participation for frozen per-session processor settings.
//!
//! This participant acquires no operating-system resources. The coordinator
//! owns the frozen settings snapshot and only its selected broker principal may
//! request a plan or callback.
use amaterasu_module_sdk::{
    ApiFieldKind as Kind, ApiMethod, ApiPrivilege, ApiSchema, Command, ModuleResult,
    extension_api::{
        self, ConsumerChange, LifecyclePhase, ProviderOperation, ProviderRequest, ProviderResponse,
        ResourcePlan,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    principal: String,
    pub(crate) service: String,
}

pub(crate) fn selection(settings: &Value) -> ModuleResult<Option<Selection>> {
    let Some(value) = settings.get("lifecycle_coordinator") else {
        return Ok(None);
    };
    let selected: Selection =
        serde_json::from_value(value.clone()).map_err(|_| "invalid lifecycle coordinator")?;
    let valid = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
    };
    if !valid(&selected.principal) || !valid(&selected.service) {
        return Err("invalid lifecycle coordinator".into());
    }
    Ok(Some(selected))
}

fn schema(required: &[(&str, Kind)], optional: &[(&str, Kind)]) -> ApiSchema {
    ApiSchema {
        required: required
            .iter()
            .cloned()
            .map(|(name, kind)| (name.into(), kind))
            .collect(),
        optional: optional
            .iter()
            .cloned()
            .map(|(name, kind)| (name.into(), kind))
            .collect::<BTreeMap<_, _>>(),
        allow_extra: false,
    }
}

pub(crate) fn registrations() -> Vec<Command> {
    ["resource_plan", "lifecycle_callback", "lifecycle_status"]
        .into_iter()
        .map(|method| Command::RegisterApi {
            method: ApiMethod {
                service: crate::settings_descriptor::SERVICE.into(),
                method: method.into(),
                interface_major: 1,
                interface_minor: 0,
                request: if method == "lifecycle_status" {
                    schema(
                        &[
                            ("provider_sequence", Kind::Integer),
                            ("transition_id", Kind::String),
                            ("operation_id", Kind::String),
                            ("receipt_id", Kind::String),
                        ],
                        &[],
                    )
                } else {
                    schema(
                        &[
                            ("provider_sequence", Kind::Integer),
                            ("operation", Kind::String),
                            ("version", Kind::Integer),
                            ("transition_id", Kind::String),
                            ("operation_id", Kind::String),
                            ("target", Kind::Object),
                            ("phase", Kind::String),
                            ("settings_revision", Kind::String),
                            ("settings", Kind::Object),
                            ("intent", Kind::Any),
                            ("reason", Kind::Any),
                            ("requested_video", Kind::Any),
                            ("consumer_requirements", Kind::Object),
                            ("remaining_ms", Kind::Integer),
                            ("plan_revision", Kind::Any),
                            ("admission_id", Kind::Any),
                            ("approved_plan", Kind::Any),
                        ],
                        &[
                            ("resource_generation", Kind::Integer),
                            ("dependency_inputs", Kind::Array),
                            ("control_results", Kind::Array),
                            ("source_video_mode", Kind::Any),
                            ("join_context", Kind::Object),
                        ],
                    )
                },
                response: schema(
                    &[("status", Kind::String)],
                    &[
                        ("reason", Kind::String),
                        ("plan", Kind::Object),
                        ("candidate", Kind::Any),
                        ("obligations", Kind::Array),
                        ("outputs", Kind::Array),
                        ("outcomes", Kind::Array),
                    ],
                ),
                privilege: ApiPrivilege::User,
            },
        })
        .collect()
}

/// Handle only synchronous, side-effect-free coordinator operations. Settings
/// stay in the coordinator's revision-bound snapshot for the selected consumer.
pub(crate) fn handle(caller: &str, selected: &Selection, payload: Value) -> Value {
    let rejected = |reason: &str| json!({"status":"rejected", "reason":reason});
    if caller != selected.principal {
        return rejected("unauthorized_caller");
    }
    let Some(envelope) = payload.as_object().filter(|value| value.len() == 4) else {
        return rejected("invalid_request");
    };
    if envelope.get("interface_major") != Some(&json!(1))
        || envelope.get("interface_minor") != Some(&json!(0))
    {
        return rejected("invalid_request");
    }
    if envelope.get("method") == Some(&json!("lifecycle_status")) {
        return json!({"status":"recovery_required", "reason":"unknown_receipt"});
    }
    let expected = match envelope.get("method").and_then(Value::as_str) {
        Some("resource_plan") => ProviderOperation::ResourcePlan,
        Some("lifecycle_callback") => ProviderOperation::LifecycleCallback,
        _ => return rejected("unknown_method"),
    };
    let Some(arguments) = envelope.get("arguments") else {
        return rejected("invalid_request");
    };
    let Ok(request) = serde_json::from_value::<ProviderRequest>(arguments.clone()) else {
        return rejected("invalid_request");
    };
    if request.operation != expected || extension_api::validate_provider_request(&request).is_err()
    {
        return rejected("invalid_request");
    }
    if crate::settings_descriptor::validate(&request.settings).is_err() {
        return rejected("invalid_settings");
    }
    if request.intent == Some(extension_api::StreamIntent::Join) {
        let response = match request.operation {
            ProviderOperation::ResourcePlan => {
                extension_api::stream_join_plan(&request, crate::settings_descriptor::PROVIDER)
                    .map(|plan| ProviderResponse::Planned { plan })
            }
            ProviderOperation::LifecycleCallback => {
                extension_api::stream_join_callback(&request, crate::settings_descriptor::PROVIDER)
            }
            _ => Err("unsupported join operation".into()),
        };
        return response
            .and_then(|response| serde_json::to_value(response).map_err(|error| error.to_string()))
            .unwrap_or_else(|_| rejected("invalid_join_context"));
    }
    let settings = json!(request.settings);
    let processing_requirement = if conversion_enabled(&settings)
        || (settings["rtx_hdr_scope"] == "global"
            && request
                .requested_video
                .as_ref()
                .is_some_and(|video| video.hdr))
    {
        json!({"source_hdr_required":false, "output_hdr_required":true})
    } else {
        json!(null)
    };
    let response = match request.operation {
        ProviderOperation::ResourcePlan => ProviderResponse::Planned {
            plan: ResourcePlan {
                plan_id: format!("rtx-hdr-settings-{}", request.provider_sequence),
                revision: request.provider_sequence,
                resources: vec![],
                mutation_domains: vec![],
                required_capabilities: vec![],
                constraints: json!({"processing_requirement": processing_requirement}),
                obligations: vec![],
                affected_consumers: vec![],
                consumer_change: ConsumerChange::Live,
                candidate_constraint: None,
                allowed_degradation: vec![],
                control_proposals: vec![],
                resource_disposition: None,
            },
        },
        ProviderOperation::LifecycleCallback
            if matches!(
                request.phase,
                LifecyclePhase::SessionRelease | LifecyclePhase::StreamRelease
            ) =>
        {
            ProviderResponse::Cleanup {
                outcomes: vec![],
                outputs: vec![],
            }
        }
        ProviderOperation::LifecycleCallback => ProviderResponse::Prepared {
            candidate: None,
            obligations: vec![],
            outputs: vec![],
        },
        _ => return rejected("invalid_operation"),
    };
    if extension_api::validate_provider_response(&request, &response).is_err() {
        return rejected("invalid_admission");
    }
    serde_json::to_value(response).unwrap_or_else(|_| rejected("invalid_response"))
}

/// The coordinator's selected consumers forward a frozen settings snapshot;
/// the provider translates its own settings into a transport-neutral intent.
pub(crate) fn intent_registration() -> Command {
    Command::RegisterApi {
        method: ApiMethod {
            service: crate::settings_descriptor::SERVICE.into(),
            method: "processing_intent".into(),
            interface_major: 1,
            interface_minor: 1,
            request: schema(
                &[
                    ("target", Kind::Object),
                    ("revision", Kind::String),
                    ("settings", Kind::Object),
                ],
                &[("output_hdr", Kind::Boolean)],
            ),
            response: schema(
                &[("status", Kind::String)],
                &[
                    ("reason", Kind::String),
                    ("target", Kind::Object),
                    ("revision", Kind::String),
                    ("enabled", Kind::Boolean),
                    ("source_hdr_required", Kind::Any),
                    ("output_hdr_required", Kind::Any),
                    ("processor_settings", Kind::Object),
                    ("processor_service", Kind::String),
                ],
            ),
            privilege: ApiPrivilege::User,
        },
    }
}

pub(crate) fn intent(caller: &str, allowed: &[String], payload: Value) -> Value {
    let reject = |reason: &str| json!({"status":"rejected", "reason":reason});
    if !allowed.iter().any(|principal| principal == caller) {
        return reject("unauthorized_caller");
    }
    let Some(envelope) = payload.as_object().filter(|object| object.len() == 4) else {
        return reject("invalid_request");
    };
    let minor = envelope.get("interface_minor").and_then(Value::as_u64);
    if envelope.get("method") != Some(&json!("processing_intent"))
        || envelope.get("interface_major") != Some(&json!(1))
        || !matches!(minor, Some(0 | 1))
    {
        return reject("invalid_request");
    }
    let Some(args) = envelope
        .get("arguments")
        .and_then(Value::as_object)
        .filter(|object| {
            object.len() == 3
                || (minor == Some(1)
                    && object.len() == 4
                    && object.get("output_hdr").is_some_and(Value::is_boolean))
        })
    else {
        return reject("invalid_request");
    };
    let Some(target) = args.get("target") else {
        return reject("invalid_request");
    };
    if serde_json::from_value::<extension_api::LifecycleTarget>(target.clone()).is_err() {
        return reject("invalid_target");
    }
    let Some(revision) = args
        .get("revision")
        .and_then(Value::as_str)
        .filter(|revision| {
            !revision.is_empty() && revision.len() <= 128 && !revision.chars().any(char::is_control)
        })
    else {
        return reject("invalid_revision");
    };
    let Some(settings) = args.get("settings") else {
        return reject("invalid_settings");
    };
    let Ok(values) = serde_json::from_value(settings.clone()) else {
        return reject("invalid_settings");
    };
    if crate::settings_descriptor::validate(&values).is_err() {
        return reject("invalid_settings");
    }
    let prepared = conversion_enabled(settings)
        || (settings["rtx_hdr_scope"] == "global"
            && args.get("output_hdr").and_then(Value::as_bool) == Some(true));
    let mut processor_settings = settings.clone();
    if let Some(object) = processor_settings.as_object_mut() {
        object.insert("foreground_target".into(), target.clone());
    }
    json!({"status":"succeeded", "target":target, "revision":revision,
        "enabled":prepared,
        "source_hdr_required":if prepared { Some(false) } else { None },
        "output_hdr_required":if prepared { Some(true) } else { None },
        "processor_settings":processor_settings, "processor_service":crate::SERVICE})
}

fn conversion_enabled(settings: &Value) -> bool {
    match settings["rtx_hdr_scope"].as_str() {
        Some("global") => settings["rtx_hdr_enable"] == true,
        Some("application") => settings["rtx_hdr_application_enable"] == true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(method: &str, operation: &str) -> Value {
        json!({"method":method,"interface_major":1,"interface_minor":0,"arguments":{
            "version":1,"provider_sequence":1,"operation":operation,
            "transition_id":"transition","operation_id":"operation",
            "target":{"kind":"stream","host_epoch":"epoch","session_id":"session","stream_id":"stream","generation":1},
            "phase":"stream.prepare","settings_revision":"revision","settings":{},
            "intent":"initial","reason":null,"requested_video":null,
            "consumer_requirements":{"execution_domain":"seat","capture_providers":[],"selector_schemas":[]},
            "remaining_ms":5000,"plan_revision":null,"admission_id":null,"approved_plan":null
        }})
    }

    #[test]
    fn selected_coordinator_can_plan_without_acquiring_resources() {
        let selected = selection(
            &json!({"lifecycle_coordinator":{"principal":"coordinator","service":"lifecycle/1"}}),
        )
        .unwrap()
        .unwrap();
        let mut request = envelope("resource_plan", "resource_plan");
        let settings =
            amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
                .unwrap();
        request["arguments"]["settings"] = json!(settings);
        assert_eq!(
            handle("other", &selected, request.clone())["reason"],
            "unauthorized_caller"
        );
        let planned = handle("coordinator", &selected, request.clone());
        assert_eq!(planned["status"], "planned");
        assert_eq!(planned["plan"]["resources"], json!([]));
        assert!(planned["plan"]["constraints"]["processing_requirement"].is_null());
        let mut enabled_request = request.clone();
        enabled_request["arguments"]["settings"]["rtx_hdr_application_enable"] = json!(true);
        let enabled_plan = handle("coordinator", &selected, enabled_request);
        assert_eq!(
            enabled_plan["plan"]["constraints"]["processing_requirement"],
            json!({"source_hdr_required":false, "output_hdr_required":true})
        );
        let mut callback = envelope("lifecycle_callback", "lifecycle_callback");
        callback["arguments"]["settings"] = request["arguments"]["settings"].clone();
        callback["arguments"]["approved_plan"] = planned["plan"].clone();
        callback["arguments"]["plan_revision"] = json!(1);
        callback["arguments"]["admission_id"] = json!("admitted");
        assert_eq!(
            handle("coordinator", &selected, callback.clone())["status"],
            "prepared"
        );
        callback["arguments"]["plan_revision"] = json!(2);
        assert_eq!(
            handle("coordinator", &selected, callback)["reason"],
            "invalid_request"
        );
    }
    #[test]
    fn unconfigured_application_does_not_reserve_processing() {
        let settings =
            amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
                .unwrap();
        let target = json!({"kind":"stream","host_epoch":"epoch","session_id":"session","stream_id":"stream","generation":1});
        for output_hdr in [false, true] {
            let request = json!({"method":"processing_intent","interface_major":1,"interface_minor":1,"arguments":{
                "target":target,"revision":"snapshot","settings":settings,"output_hdr":output_hdr
            }});
            let result = intent("consumer", &["consumer".into()], request);
            assert_eq!(result["status"], "succeeded");
            assert_eq!(result["enabled"], false);
            assert_eq!(result["processor_settings"]["rtx_hdr_enable"], false);
            assert_eq!(result["processor_settings"]["foreground_target"], target);
            assert_eq!(result["source_hdr_required"], Value::Null);
            assert_eq!(result["output_hdr_required"], Value::Null);
        }
    }

    #[test]
    fn intent_translates_frozen_settings_only_for_selected_consumers() {
        let settings =
            amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
                .unwrap();
        let target = json!({"kind":"stream","host_epoch":"epoch","session_id":"session","stream_id":"stream","generation":1});
        let mut request = json!({"method":"processing_intent","interface_major":1,"interface_minor":0,"arguments":{
            "target":target,"revision":"snapshot","settings":settings
        }});
        let allowed = vec!["stream.consumer".into()];
        assert_eq!(
            intent("other", &allowed, request.clone())["reason"],
            "unauthorized_caller"
        );
        let disabled = intent("stream.consumer", &allowed, request.clone());
        assert_eq!(disabled["status"], "succeeded");
        assert_eq!(disabled["enabled"], false);
        assert!(disabled["source_hdr_required"].is_null());
        request["arguments"]["settings"]["rtx_hdr_enable"] = json!(true);
        assert_eq!(
            intent("stream.consumer", &allowed, request.clone())["enabled"],
            false
        );
        request["arguments"]["settings"]["rtx_hdr_application_enable"] = json!(true);
        let enabled = intent("stream.consumer", &allowed, request.clone());
        assert_eq!(enabled["status"], "succeeded");
        assert_eq!(enabled["source_hdr_required"], false);
        assert_eq!(enabled["output_hdr_required"], true);
        assert_eq!(enabled["target"], target);
        assert_eq!(enabled["revision"], "snapshot");
        assert_eq!(enabled["processor_settings"]["foreground_target"], target);
        request["arguments"]["settings"]["rtx_hdr_scope"] = json!("global");
        request["arguments"]["settings"]["rtx_hdr_application_enable"] = json!(false);
        assert_eq!(
            intent("stream.consumer", &allowed, request.clone())["enabled"],
            true
        );
        request["arguments"]["settings"]["rtx_hdr_peak_brightness"] = json!(2001);
        assert_eq!(
            intent("stream.consumer", &allowed, request)["reason"],
            "invalid_settings"
        );
    }
}
