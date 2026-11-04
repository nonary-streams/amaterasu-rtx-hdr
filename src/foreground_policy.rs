//! RTX policy uses fresh, independently observed foreground facts.  A lifecycle
//! target identifies each stream; missing or ambiguous facts disable conversion.
use amaterasu_module_sdk::extension_api::{LifecycleTarget, ProviderContext};
use std::time::{Duration, Instant};

const MAX_TARGETS: usize = 16;
const MAX_OBSERVATION_AGE: Duration = Duration::from_millis(750);

#[derive(Default)]
pub(crate) struct ForegroundPolicy {
    applications: Vec<(LifecycleTarget, String)>,
    observation: Option<(String, Instant)>,
}

impl ForegroundPolicy {
    pub(crate) fn set_context(&mut self, context: &ProviderContext) {
        self.remove_target(&context.target);
        let application = &context.application;
        if application.get("kind").and_then(|v| v.as_str()) != Some("application") {
            return;
        }
        let Some(path) = application.get("executable_path").and_then(|v| v.as_str()) else {
            return;
        };
        if !valid_executable(path) || self.applications.len() >= MAX_TARGETS {
            return;
        }
        self.applications
            .push((context.target.clone(), path.to_owned()));
    }

    pub(crate) fn remove_target(&mut self, target: &LifecycleTarget) {
        self.applications.retain(|(existing, _)| existing != target);
    }

    pub(crate) fn observe(&mut self, executable: Option<&str>, now: Instant) {
        self.observation = executable
            .filter(|path| valid_executable(path))
            .map(|path| (path.to_owned(), now));
    }

    pub(crate) fn enabled(&self, target: &LifecycleTarget, now: Instant) -> bool {
        let Some((observed, captured)) = &self.observation else {
            return false;
        };
        if now
            .checked_duration_since(*captured)
            .is_none_or(|age| age > MAX_OBSERVATION_AGE)
        {
            return false;
        }
        self.applications
            .iter()
            .any(|(candidate, executable)| candidate == target && executable == observed)
    }
}

fn valid_executable(path: &str) -> bool {
    path.len() <= 4096
        && !path.chars().any(char::is_control)
        && std::path::Path::new(path).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(id: &str) -> LifecycleTarget {
        LifecycleTarget::Stream {
            host_epoch: "epoch".into(),
            session_id: "session".into(),
            stream_id: id.into(),
            generation: 1,
        }
    }
    fn context(id: &str, application: serde_json::Value) -> ProviderContext {
        ProviderContext {
            target: target(id),
            application,
            requested_video: None,
            client: None,
        }
    }
    #[test]
    fn identity_is_target_scoped_and_missing_or_stale_fails_closed() {
        let mut policy = ForegroundPolicy::default();
        let now = Instant::now();
        policy.set_context(&context(
            "one",
            json!({"kind":"application", "executable_path":"/games/one"}),
        ));
        policy.set_context(&context(
            "two",
            json!({"kind":"application", "executable_path":"/games/two"}),
        ));
        assert!(!policy.enabled(&target("one"), now));
        policy.observe(Some("/games/one"), now);
        assert!(policy.enabled(&target("one"), now));
        assert!(!policy.enabled(&target("two"), now));
        assert!(!policy.enabled(&target("one"), now + Duration::from_secs(1)));
        policy.observe(None, now);
        assert!(!policy.enabled(&target("one"), now));
        policy.set_context(&context(
            "one",
            json!({"kind":"desktop", "executable_path":"/games/one"}),
        ));
        policy.observe(Some("/games/one"), now);
        assert!(!policy.enabled(&target("one"), now));
    }
}
