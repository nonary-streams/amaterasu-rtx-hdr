//! NVIDIA TrueHDR as a host-invoked post-processing plugin.
#[cfg_attr(
    not(all(target_os = "linux", feature = "linux-nvidia")),
    allow(dead_code)
)]
mod extension;
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
    _factory: Box<amaterasu_module_sdk::processing::GpuProcessorFactory<CreateProcessor>>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    registry_principal: Option<String>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    lifecycle: Option<lifecycle::Selection>,
    #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
    processing_consumers: Vec<String>,
}

impl ManagedModule for RtxHdr {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("RTX HDR settings unavailable".into());
        };
        let registry_principal = extension::registry_principal(&payload)?;
        let lifecycle = lifecycle::selection(&payload)?;
        let processing_consumers: Vec<String> =
            match payload.get("processing_consumer_principals") {
                Some(value) => serde_json::from_value(value.clone())
                    .map_err(|_| "invalid processing consumers")?,
                None => Vec::new(),
            };
        if processing_consumers.len() > 16
            || processing_consumers.iter().any(|principal| {
                principal.is_empty()
                    || principal.len() > 128
                    || !principal.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'.' | b'_' | b'-')
                    })
            })
        {
            return Err("invalid processing consumers".into());
        }
        #[cfg(not(all(target_os = "linux", feature = "linux-nvidia")))]
        let _ = (&registry_principal, &lifecycle);
        let mut settings_payload = payload;
        if let Some(object) = settings_payload.as_object_mut() {
            object.remove("extension_registry_principal");
            object.remove("lifecycle_coordinator");
            object.remove("processing_consumer_principals");
        }
        let settings: Settings = serde_json::from_value(settings_payload)
            .map_err(|error| format!("invalid RTX HDR settings: {error}"))?;
        settings.validate()?;
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
            let create: CreateProcessor = Box::new(move |_principal| {
                Ok(linux::Processor::new(settings.clone()).with_logger(logger.clone()))
            });
            let factory = amaterasu_module_sdk::processing::GpuProcessorFactory::new(create);
            if registry_principal.is_some() {
                runtime.require_success(extension::settings_registration())?;
            }
            if lifecycle.is_some() {
                for method in lifecycle::registrations() {
                    runtime.require_success(method)?;
                }
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
            })
        }
        #[cfg(not(all(target_os = "linux", feature = "linux-nvidia")))]
        Err("RTX HDR requires a build with the linux-nvidia backend".into())
    }

    fn tick(&mut self) -> ModuleResult<()> {
        #[cfg(all(target_os = "linux", feature = "linux-nvidia"))]
        {
            self.logs.flush(&self.runtime);
            match self.runtime.send(Command::Poll {})? {
                Response::Pending => {}
                Response::Request {
                    id,
                    caller,
                    service,
                    payload,
                    ..
                } => {
                    let response = if service == settings_descriptor::SERVICE {
                        match payload.get("method").and_then(serde_json::Value::as_str) {
                            Some("settings_validate") => extension::handle_settings(
                                &caller,
                                self.registry_principal.as_deref(),
                                payload,
                            ),
                            Some("processing_intent") => {
                                lifecycle::intent(&caller, &self.processing_consumers, payload)
                            }
                            _ => match &self.lifecycle {
                                Some(selected) => lifecycle::handle(&caller, selected, payload),
                                None => {
                                    serde_json::json!({"status": "rejected", "reason": "lifecycle_not_selected"})
                                }
                            },
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

amaterasu_module_sdk::export_module!(RtxHdr);
