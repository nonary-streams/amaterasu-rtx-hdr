//! Bounded asynchronous reads of the selected foreground and lifecycle providers.
use crate::foreground_policy::ForegroundPolicy;
use amaterasu_module_sdk::{
    Command, Response, RuntimeClient,
    extension_api::{ProviderContextRequest, ProviderContextResponse, ProviderRequest},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_PENDING: usize = 16;

enum Stage {
    Submit,
    Wait(u64),
    Release(u64, Value),
}
enum Purpose {
    Observation,
    Context {
        inbound: u64,
        request: Box<ProviderRequest>,
        response: Value,
    },
}
struct Pending {
    purpose: Purpose,
    service: String,
    payload: Value,
    operation_id: String,
    deadline: Instant,
    started: Instant,
    stage: Stage,
}
pub(crate) struct Coordinator {
    policy: Arc<Mutex<ForegroundPolicy>>,
    demand: Arc<AtomicUsize>,
    service: String,
    pending: VecDeque<Pending>,
    next_poll: Instant,
    counter: u64,
    nonce: u128,
}
impl Coordinator {
    pub(crate) fn new(
        policy: Arc<Mutex<ForegroundPolicy>>,
        demand: Arc<AtomicUsize>,
        service: String,
    ) -> Self {
        Self {
            policy,
            demand,
            service,
            pending: VecDeque::new(),
            next_poll: Instant::now(),
            counter: 0,
            nonce: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        }
    }
    fn id(&mut self) -> String {
        self.counter = self.counter.wrapping_add(1);
        format!(
            "rtx-foreground-{}-{}-{}",
            std::process::id(),
            self.nonce,
            self.counter
        )
    }
    pub(crate) fn queue_context(
        &mut self,
        inbound: u64,
        request: ProviderRequest,
        response: Value,
        service: &str,
    ) -> bool {
        self.remove_target(&request.target);
        if self.pending.len() >= MAX_PENDING {
            return false;
        }
        let payload = match serde_json::to_value(ProviderContextRequest {
            operation: "read_provider_context".into(),
            provider_request: request.clone(),
        }) {
            Ok(value) => value,
            Err(_) => return false,
        };
        let operation_id = self.id();
        let deadline = Instant::now() + Duration::from_millis(request.remaining_ms.min(3000));
        self.pending.push_back(Pending {
            purpose: Purpose::Context {
                inbound,
                request: Box::new(request),
                response,
            },
            service: service.into(),
            payload,
            operation_id,
            deadline,
            started: Instant::now(),
            stage: Stage::Submit,
        });
        true
    }
    pub(crate) fn remove_target(
        &self,
        target: &amaterasu_module_sdk::extension_api::LifecycleTarget,
    ) {
        if let Ok(mut policy) = self.policy.lock() {
            policy.remove_target(target);
        }
    }
    pub(crate) fn advance(&mut self, runtime: &RuntimeClient) {
        let now = Instant::now();
        if self.demand.load(Ordering::Relaxed) == 0
            && let Ok(mut state) = self.policy.lock()
        {
            state.observe(None, now);
        }
        if self.demand.load(Ordering::Relaxed) > 0
            && now >= self.next_poll
            && self.pending.len() < MAX_PENDING
            && !self
                .pending
                .iter()
                .any(|p| matches!(p.purpose, Purpose::Observation))
        {
            let operation_id = self.id();
            self.pending.push_back(Pending {
                purpose: Purpose::Observation,
                service: self.service.clone(),
                payload: json!({"action":"snapshot"}),
                operation_id,
                deadline: now + Duration::from_millis(1000),
                started: now,
                stage: Stage::Submit,
            });
            self.next_poll = now + POLL_INTERVAL;
        }
        let Some(mut pending) = self.pending.pop_front() else {
            return;
        };
        let abandoned = match &pending.purpose {
            Purpose::Context { inbound, .. } => matches!(
                runtime.send(Command::CheckRequest { request: *inbound }),
                Ok(Response::Error { .. })
            ),
            Purpose::Observation => self.demand.load(Ordering::Relaxed) == 0,
        };
        if now >= pending.deadline || abandoned {
            if let Stage::Wait(id) | Stage::Release(id, _) = pending.stage {
                let _ = runtime.send(Command::Cancel { request: id });
                let _ = runtime.send(Command::ReleaseResult { request: id });
            }
            if matches!(pending.purpose, Purpose::Observation)
                && let Ok(mut state) = self.policy.lock()
            {
                state.observe(None, now);
            }
            if let Purpose::Context {
                inbound, response, ..
            } = pending.purpose
                && !abandoned
            {
                let _ = runtime.send(Command::Complete {
                    request: inbound,
                    payload: response,
                });
            }
            return;
        }
        match pending.stage {
            Stage::Submit => match runtime.send(Command::Request {
                service: pending.service.clone(),
                payload: pending.payload.clone(),
                timeout_ms: pending
                    .deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .min(3000) as u64,
                operation_id: pending.operation_id.clone(),
                parent: None,
            }) {
                Ok(Response::Accepted { id }) => pending.stage = Stage::Wait(id),
                Ok(Response::Error { .. }) => {
                    self.fail(pending, runtime, now);
                    return;
                }
                _ => pending.stage = Stage::Submit,
            },
            Stage::Wait(id) => match runtime.send(Command::TakeResult { request: id }) {
                Ok(Response::Result { payload }) => pending.stage = Stage::Release(id, payload),
                Ok(Response::Error { .. }) => {
                    self.fail(pending, runtime, now);
                    return;
                }
                _ => pending.stage = Stage::Wait(id),
            },
            Stage::Release(id, ref payload) => {
                match runtime.send(Command::ReleaseResult { request: id }) {
                    Ok(Response::Ok) | Ok(Response::Error { .. }) => {
                        self.finish(pending.purpose, payload.clone(), runtime, pending.started);
                        return;
                    }
                    _ => (),
                }
            }
        }
        self.pending.push_back(pending);
    }
    fn fail(&self, pending: Pending, runtime: &RuntimeClient, now: Instant) {
        match pending.purpose {
            Purpose::Observation => {
                if self.demand.load(Ordering::Relaxed) == 0 {
                    return;
                }
                if let Ok(mut state) = self.policy.lock() {
                    state.observe(None, now);
                }
            }
            Purpose::Context {
                inbound, response, ..
            } => {
                let _ = runtime.send(Command::Complete {
                    request: inbound,
                    payload: response,
                });
            }
        }
    }
    fn finish(&self, purpose: Purpose, payload: Value, runtime: &RuntimeClient, now: Instant) {
        match purpose {
            Purpose::Observation => {
                let observation =
                    amaterasu_module_sdk::foreground_context::SnapshotResponse::parse(&payload)
                        .ok()
                        .and_then(|reply| reply.snapshot);
                if let Ok(mut state) = self.policy.lock() {
                    state.observe(observation.as_ref(), now);
                }
            }
            Purpose::Context {
                inbound,
                request,
                response,
            } => {
                let valid = serde_json::from_value::<ProviderContextResponse>(payload)
                    .ok()
                    .filter(|result| {
                        result.status == "succeeded"
                            && result.context.target == request.target
                            && result.context.requested_video == request.requested_video
                    });
                if let Some(context) = valid
                    && let Ok(mut policy) = self.policy.lock()
                {
                    policy.set_context(&context.context);
                }
                let _ = runtime.send(Command::Complete {
                    request: inbound,
                    payload: response,
                });
            }
        }
    }
}
