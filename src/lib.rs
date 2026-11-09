//! NVIDIA TrueHDR as a host-invoked post-processing plugin.
#[cfg_attr(
    not(all(target_os = "linux", feature = "linux-nvidia")),
    allow(dead_code)
)]
mod display_adjustment;
#[cfg_attr(
    not(all(target_os = "linux", feature = "linux-nvidia")),
    allow(dead_code)
)]
mod extension;
#[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
mod foreground_coordinator;
#[allow(dead_code)]
mod foreground_policy;
#[cfg_attr(
    not(all(target_os = "linux", feature = "linux-nvidia")),
    allow(dead_code)
)]
mod lifecycle;
#[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
mod linux;
#[cfg_attr(
    not(all(target_os = "linux", feature = "linux-nvidia")),
    allow(dead_code)
)]
mod settings;
pub mod settings_descriptor;
pub use settings::Settings;

use amaterasu_module_sdk::{Command, ManagedModule, ModuleResult, Response, RuntimeClient};

pub const SERVICE: &str = "amaterasu.rtx-hdr/1";

#[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
type CreateProcessor =
    Box<dyn Fn(&str) -> Result<linux::Processor, amaterasu_module_sdk::abi::Status> + Send + Sync>;

pub struct RtxHdr {
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    runtime: RuntimeClient,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    logs: amaterasu_module_sdk::LogQueue,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    _factory: Box<amaterasu_module_sdk::frame_processing::FrameProcessorFactory<CreateProcessor>>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    registry_principal: Option<String>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    lifecycle: Option<lifecycle::Selection>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    processing_consumers: Vec<String>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    foreground: Option<foreground_coordinator::Coordinator>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    display_adjustment: Option<display_adjustment::Coordinator>,
}

impl ManagedModule for RtxHdr {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("RTX HDR settings unavailable".into());
        };
        use amaterasu_module_sdk::processing_composition as composition_sdk;
        let composition = runtime
            .processing_composition()?
            .ok_or("RTX HDR processing composition unavailable")?;
        let processor = composition
            .processor
            .as_ref()
            .ok_or("RTX HDR processor not selected")?;
        composition_sdk::require_contract(
            processor,
            amaterasu_module_sdk::media::native_contracts::FRAME_PROCESSOR,
            1,
            0,
        )?;
        if processor.service != SERVICE {
            return Err("selected RTX HDR processor service does not match provider".into());
        }
        if let Some(control) = &composition.processor_control {
            composition_sdk::require_contract(
                control,
                composition_sdk::PROCESSING_INTENT_CONTRACT,
                1,
                1,
            )?;
            if control.principal != processor.principal
                || control.service != settings_descriptor::SERVICE
            {
                return Err("selected RTX HDR control does not match provider".into());
            }
        } else if composition.stream_owner.is_some()
            || composition.lifecycle.is_some()
            || composition.settings_registry.is_some()
            || composition.display_settings.is_some()
        {
            return Err("managed RTX HDR requires a processing control contract".into());
        }
        let lifecycle = lifecycle::selection(composition.lifecycle.as_ref())?;
        let registry_principal =
            extension::registry_principal(composition.settings_registry.as_ref())?;
        let display_service = composition
            .display_settings
            .as_ref()
            .map(|binding| {
                composition_sdk::require_contract(
                    binding,
                    amaterasu_module_sdk::display_settings::SERVICE,
                    1,
                    0,
                )
                .map(str::to_owned)
            })
            .transpose()?;
        match (&display_service, &composition.source_preparation) {
            (Some(_), Some(binding)) => {
                composition_sdk::require_contract(
                    binding,
                    composition_sdk::SOURCE_PREPARATION_CONTRACT,
                    1,
                    0,
                )?;
                if binding.principal != processor.principal
                    || binding.service != settings_descriptor::SERVICE
                {
                    return Err("selected source preparation does not match provider".into());
                }
            }
            (None, None) => {}
            _ => return Err("source preparation requires display settings".into()),
        }
        let foreground_service = composition
            .foreground
            .as_ref()
            .map(|binding| {
                composition_sdk::require_contract(
                    binding,
                    amaterasu_module_sdk::foreground_context::SERVICE,
                    1,
                    0,
                )
                .map(str::to_owned)
            })
            .transpose()?;
        let mut processing_consumers: Vec<String> = composition
            .stream_owner
            .iter()
            .chain(composition.lifecycle.iter())
            .map(|binding| binding.principal.clone())
            .collect();
        processing_consumers.sort();
        processing_consumers.dedup();
        let settings: Settings = serde_json::from_value(payload)
            .map_err(|error| format!("invalid RTX HDR settings: {error}"))?;
        settings.validate()?;
        require_foreground_provider(&settings, foreground_service.as_deref())?;
        #[cfg(not(all(target_os = "linux", feature = "linux-nvidia")))]
        let _ = (
            &registry_principal,
            &lifecycle,
            &display_service,
            &processing_consumers,
        );
        #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
        {
            if settings.require_verified_runtime {
                let parent = settings
                    .runtime_directory
                    .parent()
                    .ok_or("RTX HDR runtime parent unavailable")?;
                let verified = amaterasu_rtx_runtime::verify(
                    parent,
                    &std::sync::atomic::AtomicBool::new(false),
                )
                .map_err(|error| format!("RTX HDR runtime verification failed: {error}"))?;
                let expected = std::fs::canonicalize(&settings.runtime_directory)
                    .map_err(|error| format!("RTX HDR runtime directory unavailable: {error}"))?;
                if std::fs::canonicalize(verified).map_err(|error| error.to_string())? != expected {
                    return Err("RTX HDR verified runtime differs from selected directory".into());
                }
            }
            let settings = Settings {
                runtime_directory: std::fs::canonicalize(&settings.runtime_directory)
                    .map_err(|error| format!("RTX HDR runtime directory unavailable: {error}"))?,
                ..settings
            };
            let (logger, logs) = runtime.log_emitter();
            let policy = std::sync::Arc::new(std::sync::Mutex::new(
                foreground_policy::ForegroundPolicy::default(),
            ));
            let demand = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let processor_demand = demand.clone();
            let processor_policy = policy.clone();
            let has_foreground = foreground_service.is_some();
            let foreground = foreground_service
                .map(|service| foreground_coordinator::Coordinator::new(policy, demand, service));
            let create: CreateProcessor = Box::new(move |_principal| {
                let processor = linux::Processor::new(settings.clone()).with_logger(logger.clone());
                Ok(if has_foreground {
                    processor.with_policy(processor_policy.clone(), processor_demand.clone())
                } else {
                    processor
                })
            });
            let factory =
                amaterasu_module_sdk::frame_processing::FrameProcessorFactory::new(create);
            if registry_principal.is_some() {
                runtime.require_success(extension::settings_registration())?;
            }
            if lifecycle.is_some() {
                for method in lifecycle::registrations() {
                    runtime.require_success(method)?;
                }
            }
            let display_adjustment = display_service.map(|service| {
                display_adjustment::Coordinator::new(service, processing_consumers.clone())
            });
            if display_adjustment.is_some() {
                runtime.require_success(Command::RegisterApi {
                    method: amaterasu_module_sdk::display_settings::preparation::method(
                        settings_descriptor::SERVICE,
                    ),
                })?;
            }
            if !processing_consumers.is_empty() {
                runtime.require_success(lifecycle::intent_registration())?;
            }
            runtime.require_success(Command::Register {
                service: SERVICE.into(),
            })?;
            // SAFETY: the boxed SDK factory lives through this module. Signed
            // composition must stop video/encoder consumers before this plugin.
            unsafe { factory.register(&runtime, SERVICE) }?;
            Ok(Self {
                _factory: factory,
                runtime,
                logs,
                registry_principal,
                lifecycle,
                processing_consumers,
                foreground,
                display_adjustment,
            })
        }
        #[cfg(not(all(target_os = "linux", feature = "linux-nvidia")))]
        Err("RTX HDR requires a build with the linux-nvidia backend".into())
    }

    fn quiesce(&mut self) -> ModuleResult<bool> {
        self.tick()?;
        #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
        {
            let frames_retired = linux::retirement_ready()
                .map_err(|status| format!("RTX frame resources did not retire: {status}"))?;
            Ok(frames_retired
                && self
                    .display_adjustment
                    .as_ref()
                    .is_none_or(|coordinator| coordinator.is_idle()))
        }
        #[cfg(not(all(target_os = "linux", feature = "linux-nvidia")))]
        Ok(true)
    }

    fn tick(&mut self) -> ModuleResult<()> {
        #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
        {
            self.logs.flush(&self.runtime);
            if let Some(coordinator) = &mut self.display_adjustment {
                coordinator.advance(&self.runtime);
            }
            if let Some(foreground) = &mut self.foreground {
                foreground.advance(&self.runtime);
            }
            match self.runtime.send(Command::Poll {})? {
                Response::Pending => {}
                Response::Request {
                    id,
                    caller,
                    service,
                    payload,
                    remaining_ms,
                    ..
                } => {
                    if service == settings_descriptor::SERVICE
                        && payload["method"] == "prepare_source"
                    {
                        let response = match &mut self.display_adjustment {
                            Some(coordinator) => {
                                match coordinator.enqueue(id, &caller, payload, remaining_ms) {
                                    Ok(None) => return Ok(()),
                                    Ok(Some(response)) => response,
                                    Err(code) => serde_json::json!({"ok":false,"code":code}),
                                }
                            }
                            None => {
                                serde_json::json!({"ok":false,"code":"display_settings_not_selected"})
                            }
                        };
                        self.runtime.send(Command::Complete {
                            request: id,
                            payload: response,
                        })?;
                        return Ok(());
                    }
                    let response = if service == settings_descriptor::SERVICE {
                        match payload.get("method").and_then(serde_json::Value::as_str) {
                            Some("settings_validate") => extension::handle_settings(
                                &caller,
                                self.registry_principal.as_deref(),
                                self.foreground.is_some(),
                                payload,
                            ),
                            Some("processing_intent") => {
                                lifecycle::intent(&caller, &self.processing_consumers, payload)
                            }
                            _ => {
                                match &self.lifecycle {
                                    Some(selected) => {
                                        let response =
                                            lifecycle::handle(&caller, selected, payload.clone());
                                        let request = payload.get("arguments").and_then(|arguments| serde_json::from_value::<amaterasu_module_sdk::extension_api::ProviderRequest>(arguments.clone()).ok());
                                        if response
                                            .get("status")
                                            .and_then(serde_json::Value::as_str)
                                            == Some("cleanup")
                                            && let (Some(coordinator), Some(request)) =
                                                (&self.foreground, &request)
                                        {
                                            coordinator.remove_target(&request.target);
                                        }
                                        if response.get("status").and_then(serde_json::Value::as_str) == Some("prepared")
                                        && request.as_ref().is_some_and(|request| request.phase == amaterasu_module_sdk::extension_api::LifecyclePhase::StreamPrepare)
                                        && let (Some(coordinator), Some(request)) = (&mut self.foreground, request)
                                            && coordinator.queue_context(id, request, response.clone(), &selected.service) { return Ok(()); }
                                        response
                                    }
                                    None => {
                                        serde_json::json!({"status": "rejected", "reason": "lifecycle_not_selected"})
                                    }
                                }
                            }
                        }
                    } else {
                        serde_json::json!({"ok": false, "error": "unknown_service"})
                    };
                    match self.runtime.send(Command::Complete {
                        request: id,
                        payload: response,
                    })? {
                        Response::Ok => {}
                        Response::Error { code, .. }
                            if code == "request_ended" || code == "not_found" => {}
                        other => return Err(format!("RTX HDR completion failed: {other:?}")),
                    }
                }
                other => return Err(format!("RTX HDR poll failed: {other:?}")),
            }
        }
        Ok(())
    }
}

/// Foreground and fullscreen gates can only be evaluated through a selected
/// provider, so a composition without one cannot accept those settings.
fn require_foreground_provider(
    settings: &Settings,
    foreground_service: Option<&str>,
) -> ModuleResult<()> {
    if foreground_service.is_none() && (settings.foreground_only || settings.fullscreen_only) {
        return Err("foreground restrictions require a selected foreground provider".into());
    }
    Ok(())
}

amaterasu_module_sdk::export_module!(RtxHdr);

#[cfg(test)]
mod foreground_selection_tests {
    use super::*;
    use amaterasu_module_sdk::processing_composition::ProcessingComposition;

    fn settings(foreground_only: bool) -> Settings {
        serde_json::from_value(serde_json::json!({
            "runtime_directory": "/tmp/rtx-runtime",
            "foreground_only": foreground_only,
        }))
        .unwrap()
    }

    fn migrated_policy() -> serde_json::Value {
        let path = std::env::var("AMATERASU_MIGRATED_REPORT").unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        report["optional_bootstrap"]["policy"].clone()
    }

    #[test]
    fn foreground_only_settings_need_a_selected_provider() {
        let service = amaterasu_module_sdk::foreground_context::SERVICE;
        assert!(require_foreground_provider(&settings(false), None).is_ok());
        assert!(require_foreground_provider(&settings(true), None).is_err());
        assert!(require_foreground_provider(&settings(true), Some(service)).is_ok());
    }

    #[test]
    #[ignore = "needs AMATERASU_MIGRATED_REPORT from the release review"]
    fn reviewed_installed_policy_starts_rtx_hdr() {
        let policy = migrated_policy();
        let id = "amaterasu.rtx-hdr";
        let parsed: Settings = serde_json::from_value(policy["settings"][id].clone()).unwrap();
        parsed.validate().unwrap();
        let composition: ProcessingComposition =
            serde_json::from_value(policy["processing_compositions"][id].clone()).unwrap();
        let foreground = composition.foreground.as_ref().unwrap();
        assert_eq!(
            foreground.service,
            amaterasu_module_sdk::foreground_context::SERVICE
        );
        let persisted = settings(true);
        assert!(require_foreground_provider(&persisted, Some(&foreground.service)).is_ok());
    }
}
