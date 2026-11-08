//! RTX HDR owns its display policy through the public Display Settings SDK.
use amaterasu_module_sdk::{
    Command, Response, RuntimeClient,
    display_settings::{Adjustment, HdrMode, Operation, preparation::Context},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
enum Stage {
    Submit,
    Wait(u64),
    Release(u64, Value),
    Complete(Value),
}
struct Pending {
    inbound: u64,
    context: Context,
    command: Command,
    deadline: Instant,
    cancelled: bool,
    stage: Stage,
}
pub(crate) struct Coordinator {
    service: String,
    callers: Vec<String>,
    pending: VecDeque<Pending>,
}
impl Coordinator {
    pub fn new(service: String, callers: Vec<String>) -> Self {
        Self {
            service,
            callers,
            pending: VecDeque::new(),
        }
    }
    pub fn enqueue(
        &mut self,
        inbound: u64,
        caller: &str,
        payload: Value,
        remaining_ms: u64,
    ) -> Result<Option<Value>, String> {
        if !self.callers.iter().any(|c| c == caller) {
            return Err("permission_denied".into());
        }
        if self.pending.len() >= 16 {
            return Err("display_preparation_capacity".into());
        }
        if payload["interface_major"] != 1
            || payload["interface_minor"] != 0
            || payload["method"] != "prepare_source"
        {
            return Err("invalid_source_preparation".into());
        }
        let context: Context = serde_json::from_value(payload["arguments"]["context"].clone())
            .map_err(|_| "invalid_source_context")?;
        if context.settings_service != self.service
            || context.settings_revision.is_empty()
            || context.settings_revision.len() > 128
        {
            return Err("invalid_source_context".into());
        }
        let values =
            serde_json::from_value(context.settings.clone()).map_err(|_| "invalid_settings")?;
        crate::settings_descriptor::validate(&values)?;
        let active = crate::lifecycle::prepares_conversion(
            &context.settings,
            Some(context.output_format.dynamic_range == amaterasu_module_sdk::DynamicRange::Hdr),
        );
        if !active {
            return Ok(Some(json!({"ok":true,"scope":context.display_scope})));
        }
        let operation_id = format!("rtx-source-{}", context.display_scope.id);
        let command = Operation::Acquire {
            operation_id: operation_id.clone(),
            scope: context.display_scope.clone(),
            adjustment: Adjustment::new()
                .hdr(HdrMode::Disabled)
                .wide_color_gamut(false),
        }
        .command(amaterasu_module_sdk::requests::RequestOptions {
            service: &self.service,
            timeout_ms: remaining_ms.min(5000),
            operation_id: &operation_id,
            parent: Some(inbound),
        })?;
        self.pending.push_back(Pending {
            inbound,
            context,
            command,
            deadline: Instant::now() + Duration::from_millis(remaining_ms.min(5000)),
            cancelled: false,
            stage: Stage::Submit,
        });
        Ok(None)
    }
    pub fn advance(&mut self, runtime: &RuntimeClient) {
        self.advance_with(&mut |command| runtime.send(command));
    }
    fn advance_with(&mut self, send: &mut impl FnMut(Command) -> Result<Response, String>) {
        let Some(mut pending) = self.pending.pop_front() else {
            return;
        };
        if Instant::now() >= pending.deadline
            || !matches!(
                send(Command::CheckRequest {
                    request: pending.inbound
                }),
                Ok(Response::Ok)
            )
        {
            pending.cancelled = true;
        }
        let mut finished = None;
        match &pending.stage {
            Stage::Complete(payload) => {
                match send(Command::Complete {
                    request: pending.inbound,
                    payload: payload.clone(),
                }) {
                    Ok(Response::Ok) => return,
                    Ok(Response::Error { code, .. })
                        if code == "request_ended" || code == "not_found" =>
                    {
                        return;
                    }
                    _ => {}
                }
            }
            Stage::Submit if pending.cancelled => {
                finished = Some(json!({"ok":false,"code":"source_preparation_cancelled"}))
            }
            Stage::Submit => match send(pending.command.clone()) {
                Ok(Response::Accepted { id }) => pending.stage = Stage::Wait(id),
                Ok(Response::Error { .. }) => {
                    finished = Some(json!({"ok":false,"code":"display_adjustment_unavailable"}))
                }
                _ => {}
            },
            Stage::Wait(id) => {
                if pending.cancelled {
                    let _ = send(Command::Cancel { request: *id });
                }
                match send(Command::TakeResult { request: *id }) {
                    Ok(Response::Result { payload }) => {
                        pending.stage = Stage::Release(*id, payload)
                    }
                    Ok(Response::Error { code, .. })
                        if code == "request_ended" || code == "not_found" =>
                    {
                        finished = Some(json!({"ok":false,"code":"display_adjustment_cancelled"}))
                    }
                    _ => {}
                }
            }
            Stage::Release(id, payload) => {
                if matches!(
                    send(Command::ReleaseResult { request: *id }),
                    Ok(Response::Ok)
                ) {
                    let success = !pending.cancelled
                        && amaterasu_module_sdk::display_settings::lease_from_reply(
                            payload.clone(),
                            &pending.context.display_scope,
                        )
                        .is_ok();
                    finished = Some(if success {
                        json!({"ok":true,"scope":pending.context.display_scope})
                    } else {
                        json!({"ok":false,"code":"display_adjustment_unconfirmed"})
                    });
                }
            }
        }
        if let Some(payload) = finished {
            pending.stage = Stage::Complete(payload);
        }
        self.pending.push_back(pending);
    }
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_module_sdk::display_settings;
    fn context() -> Context {
        let mut settings =
            amaterasu_configuration_schema::defaults(&crate::settings_descriptor::definitions())
                .unwrap();
        settings.insert("rtx_hdr_scope".into(), json!("global"));
        Context{display_scope:display_settings::Scope{id:"scope".into(),target:display_settings::Target{id:"display".into(),generation:"one".into()}},settings_service:display_settings::SERVICE.into(),lifecycle_target:serde_json::from_value(json!({"kind":"stream","host_epoch":"e","session_id":"s","stream_id":"t","generation":1})).unwrap(),output_format:amaterasu_module_sdk::VideoStreamFormat{codec:amaterasu_module_sdk::VideoCodec::Hevc,bit_depth:10,dynamic_range:amaterasu_module_sdk::DynamicRange::Hdr},settings_revision:"r".into(),settings:json!(settings)}
    }
    #[test]
    fn sdr_output_does_not_request_a_display_adjustment() {
        let mut coordinator =
            Coordinator::new(display_settings::SERVICE.into(), vec!["host".into()]);
        let mut context = context();
        context.output_format.dynamic_range = amaterasu_module_sdk::DynamicRange::Sdr;
        context.settings["rtx_hdr_enable"] = json!(true);
        let response = coordinator
            .enqueue(
                1,
                "host",
                display_settings::preparation::payload(&context),
                5000,
            )
            .unwrap()
            .unwrap();
        assert_eq!(response["ok"], true);
        assert!(coordinator.is_idle());
    }
    #[test]
    fn plugin_requests_and_confirms_sdr_through_public_sdk() {
        let mut coordinator =
            Coordinator::new(display_settings::SERVICE.into(), vec!["host".into()]);
        let context = context();
        assert!(
            coordinator
                .enqueue(
                    1,
                    "host",
                    display_settings::preparation::payload(&context),
                    5000
                )
                .unwrap()
                .is_none()
        );
        let mut finished = false;
        let mut requested = false;
        let mut send = |command| {
            Ok(match command {
                Command::CheckRequest { .. } => Response::Ok,
                Command::CallApi {
                    service,
                    method,
                    payload,
                    ..
                } => {
                    assert_eq!(service, display_settings::SERVICE);
                    assert_eq!(method, "acquire");
                    assert_eq!(payload["adjustment"]["hdr"], "disabled");
                    assert_eq!(payload["adjustment"]["wide_color_gamut"], false);
                    assert_eq!(payload["scope"], json!(context.display_scope));
                    requested = true;
                    Response::Accepted { id: 2 }
                }
                Command::TakeResult { .. } => Response::Result {
                    payload: json!({"ok":true,"enforced":true,"lease":{"id":"lease","scope":context.display_scope}}),
                },
                Command::ReleaseResult { .. } => Response::Ok,
                Command::Complete { payload, .. } => {
                    assert_eq!(payload["ok"], true);
                    finished = true;
                    Response::Ok
                }
                _ => panic!("unexpected command"),
            })
        };
        for _ in 0..5 {
            coordinator.advance_with(&mut send);
        }
        assert!(requested && finished && coordinator.is_idle());
    }

    #[test]
    fn same_plugin_uses_each_selected_display_settings_implementation() {
        trait Provider {
            fn acquire(&mut self, arguments: Value) -> Value;
        }
        struct DirectProvider;
        impl Provider for DirectProvider {
            fn acquire(&mut self, arguments: Value) -> Value {
                assert_eq!(arguments["adjustment"]["hdr"], "disabled");
                assert_eq!(arguments["adjustment"]["wide_color_gamut"], false);
                json!({"ok":true,"enforced":true,"lease":{"id":"direct","scope":arguments["scope"]}})
            }
        }
        struct TypedProvider;
        impl Provider for TypedProvider {
            fn acquire(&mut self, mut arguments: Value) -> Value {
                arguments["action"] = json!("acquire");
                let operation: display_settings::Operation =
                    serde_json::from_value(arguments).unwrap();
                let display_settings::Operation::Acquire {
                    scope, adjustment, ..
                } = operation
                else {
                    panic!("wrong operation");
                };
                adjustment.validate().unwrap();
                assert_eq!(adjustment.hdr, Some(display_settings::HdrMode::Disabled));
                assert_eq!(adjustment.wide_color_gamut, Some(false));
                json!({"ok":true,"enforced":true,"lease":{"id":"typed","scope":scope}})
            }
        }
        for (service, mut implementation) in [
            (
                "example.first-display/1",
                Box::new(DirectProvider) as Box<dyn Provider>,
            ),
            (
                "example.second-display/1",
                Box::new(TypedProvider) as Box<dyn Provider>,
            ),
        ] {
            let mut coordinator = Coordinator::new(service.into(), vec!["host".into()]);
            let mut context = context();
            context.settings_service = service.into();
            coordinator
                .enqueue(
                    1,
                    "host",
                    display_settings::preparation::payload(&context),
                    5000,
                )
                .unwrap();
            let mut response = None;
            let mut completed = false;
            let mut broker = |command| {
                Ok(match command {
                    Command::CheckRequest { .. } => Response::Ok,
                    Command::CallApi {
                        service: actual,
                        method,
                        payload,
                        ..
                    } => {
                        assert_eq!(actual, service);
                        assert_eq!(method, "acquire");
                        response = Some(implementation.acquire(payload));
                        Response::Accepted { id: 2 }
                    }
                    Command::TakeResult { .. } => Response::Result {
                        payload: response.take().unwrap(),
                    },
                    Command::ReleaseResult { .. } => Response::Ok,
                    Command::Complete { payload, .. } => {
                        assert_eq!(payload["ok"], true);
                        completed = true;
                        Response::Ok
                    }
                    _ => panic!("unexpected command"),
                })
            };
            for _ in 0..5 {
                coordinator.advance_with(&mut broker);
            }
            assert!(completed && coordinator.is_idle());
        }
    }
    #[test]
    fn unselected_service_and_caller_cannot_request_adjustments() {
        let mut coordinator = Coordinator::new("selected/1".into(), vec!["host".into()]);
        let p = display_settings::preparation::payload(&context());
        assert!(coordinator.enqueue(1, "other", p.clone(), 5000).is_err());
        assert!(coordinator.enqueue(1, "host", p, 5000).is_err());
    }
}
