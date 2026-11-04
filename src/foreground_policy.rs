//! RTX policy uses fresh, independently observed foreground facts.  A lifecycle
//! target identifies each stream; missing or ambiguous facts disable conversion.
use amaterasu_module_sdk::extension_api::{LifecycleTarget, ProviderContext};
use std::time::{Duration, Instant};

const MAX_TARGETS: usize = 16;
const MAX_OBSERVATION_AGE: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub(crate) struct ForegroundPolicy {
    applications: Vec<(LifecycleTarget, String)>,
    observation: Option<(String, Option<bool>, Instant)>,
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

    pub(crate) fn observe(
        &mut self,
        snapshot: Option<&amaterasu_module_sdk::foreground_context::Snapshot>,
        now: Instant,
    ) {
        self.observation = snapshot
            .filter(|value| valid_executable(&value.executable))
            .map(|value| (value.executable.clone(), value.fullscreen, now));
    }

    pub(crate) fn enabled(
        &self,
        target: Option<&LifecycleTarget>,
        foreground_only: bool,
        fullscreen_only: bool,
        now: Instant,
    ) -> bool {
        if !foreground_only && !fullscreen_only {
            return true;
        }
        let Some((observed, fullscreen, captured)) = &self.observation else {
            return false;
        };
        if now
            .checked_duration_since(*captured)
            .is_none_or(|age| age > MAX_OBSERVATION_AGE)
        {
            return false;
        }
        if fullscreen_only && *fullscreen != Some(true) {
            return false;
        }
        !foreground_only
            || target.is_some_and(|target| {
                self.applications
                    .iter()
                    .any(|(candidate, executable)| candidate == target && executable == observed)
            })
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
    fn snapshot(fullscreen: Option<bool>) -> amaterasu_module_sdk::foreground_context::Snapshot {
        amaterasu_module_sdk::foreground_context::Snapshot {
            executable: "/games/one".into(),
            fullscreen,
        }
    }
    #[test]
    fn fullscreen_requires_fresh_explicit_true_and_combines_with_identity() {
        let mut policy = ForegroundPolicy::default();
        let now = Instant::now();
        assert!(policy.enabled(None, false, false, now));
        assert!(!policy.enabled(None, false, true, now));
        for value in [None, Some(false)] {
            policy.observe(Some(&snapshot(value)), now);
            assert!(!policy.enabled(None, false, true, now));
        }
        policy.observe(Some(&snapshot(Some(true))), now);
        assert!(policy.enabled(None, false, true, now));
        assert!(!policy.enabled(Some(&target("one")), true, true, now));
        policy.set_context(&context(
            "one",
            json!({"kind":"application", "executable_path":"/games/one"}),
        ));
        assert!(policy.enabled(Some(&target("one")), true, true, now));
        assert!(!policy.enabled(None, false, true, now + Duration::from_secs(2)));
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
        assert!(!policy.enabled(Some(&target("one")), true, false, now));
        policy.observe(Some(&snapshot(Some(true))), now);
        assert!(policy.enabled(Some(&target("one")), true, false, now));
        assert!(!policy.enabled(Some(&target("two")), true, false, now));
        assert!(!policy.enabled(
            Some(&target("one")),
            true,
            false,
            now + Duration::from_secs(2)
        ));
        policy.observe(None, now);
        assert!(!policy.enabled(Some(&target("one")), true, false, now));
        policy.set_context(&context(
            "one",
            json!({"kind":"desktop", "executable_path":"/games/one"}),
        ));
        policy.observe(Some(&snapshot(Some(true))), now);
        assert!(!policy.enabled(Some(&target("one")), true, false, now));
    }
}
