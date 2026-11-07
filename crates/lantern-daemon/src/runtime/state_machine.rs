//! Pure supervisor transitions and operation preemption policy.

use lantern_core::model::{InstanceStatus, IssueCode};

/// An operation serialized by the instance supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Start an instance.
    Start,
    /// Stop an instance or cancel its preparation.
    Stop,
    /// Stop and start an instance.
    Restart,
    /// Prepare a checkout without starting its server.
    Prepare,
}

/// Decision for a request arriving while another operation is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPolicy {
    /// No operation is running; begin the new one.
    Run,
    /// Identical operation: join its existing result.
    Join,
    /// Cancel the old operation before running Stop.
    Preempt,
    /// Return `operation_in_progress`.
    Busy,
}

/// Apply decision D18: only identical requests join, and Stop preempts other work.
pub fn request_policy(inflight: Option<Operation>, request: Operation) -> RequestPolicy {
    match inflight {
        None => RequestPolicy::Run,
        Some(op) if op == request => RequestPolicy::Join,
        Some(Operation::Start | Operation::Restart | Operation::Prepare) if request == Operation::Stop => {
            RequestPolicy::Preempt
        }
        Some(_) => RequestPolicy::Busy,
    }
}

/// Input validation already performed by the registry and launch planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartValidation {
    /// Enabled and a valid launch plan exists.
    Ready,
    /// The project is disabled.
    Disabled,
    /// The manifest or plan needs correction.
    Invalid(IssueCode),
}

/// The kind of plan selected after dependencies, port and preparation succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartKind {
    /// Start a regular transient service.
    Unit,
    /// Run a deploy-start one-shot.
    Deploy,
    /// Check and publish a process-less site.
    Site,
}

/// Why a request is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// A different operation is already running.
    OperationInProgress,
    /// The project is disabled.
    InstanceDisabled,
    /// The plan is invalid.
    NeedsConfig,
    /// A foreign listener is already running.
    ExternallyRunning,
    /// A foreign listener cannot be stopped by Lantern.
    NotManaged,
    /// This operation is not available in the current status.
    InvalidState,
    /// The service did not stop by its deadline.
    StopTimeout,
}

/// Serializable state needed by the pure supervisor transition function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorState {
    /// Public lifecycle status.
    pub status: InstanceStatus,
    /// Whether the intended state is running.
    pub desired_up: bool,
    /// The currently executing user operation, if any.
    pub inflight: Option<Operation>,
    /// Whether a crash retry timer is scheduled.
    pub retry_pending: bool,
    /// Whether a completed stop should begin a restart.
    pub restart_pending: bool,
    /// Count of automatic retries.
    pub restarts: u32,
    /// Current issue code, if any.
    pub issue: Option<IssueCode>,
    /// Human-readable status detail, if any.
    pub message: Option<String>,
}

impl Default for SupervisorState {
    fn default() -> Self {
        Self {
            status: InstanceStatus::Stopped,
            desired_up: false,
            inflight: None,
            retry_pending: false,
            restart_pending: false,
            restarts: 0,
            issue: None,
            message: None,
        }
    }
}

/// Inputs to the supervisor; OS work must feed results back as events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A user operation and the current plan validation result.
    Request {
        operation: Operation,
        validation: StartValidation,
    },
    /// Dependencies, lease and checkout preparation finished successfully.
    Prepared(StartKind),
    /// A preparation or spawn failed before a unit was started.
    PreparationFailed(IssueCode),
    /// A cycle, missing dependency or invalid manifest was detected.
    PlanInvalid(IssueCode),
    /// The server or site passed readiness.
    Ready,
    /// The process exited; `clean` is ignored before readiness.
    Exited {
        clean: bool,
        retry: bool,
        issue: Option<IssueCode>,
    },
    /// A wrong-port listener was observed at the startup deadline.
    PortMismatch,
    /// Nothing listened before the startup deadline.
    ReadinessTimeout,
    /// A deploy-start command failed.
    DeployFailed(IssueCode),
    /// The health failure threshold was reached.
    HealthFailed,
    /// Health recovered.
    HealthRecovered,
    /// A managed unit finished stopping or was absent.
    UnitInactive,
    /// Cleanup after a failed start could not stop the unit.
    CleanupFailed(IssueCode),
    /// A stop deadline expired while processes were still present.
    StopTimeout,
    /// A deploy stop left the externally managed listener alive.
    StopIncomplete,
    /// A scheduled crash retry fired.
    RetryTimer,
    /// Discovery found a matched unmanaged listener.
    ExternalAppeared,
    /// Discovery no longer finds the matched listener.
    ExternalDisappeared,
    /// Reconciliation found a live managed unit.
    ReconcileUnit { ready: bool },
    /// Reconciliation found an activated process-less site.
    ReconcileSite,
    /// The manifest changed; `valid` signals recovery from NeedsConfig.
    ManifestChanged { valid: bool },
    /// Publishing a route failed while a process remains available.
    RouteFailed,
}

/// Work for the supervisor's effect runner; no effect executes in `transition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Join the in-flight operation.
    Join(Operation),
    /// Cancel the in-flight operation and its child one-shot, if any.
    CancelOperation(Operation),
    /// Reject a request or failed stop.
    Reject(RequestError),
    /// Begin dependency, port, environment and checkout preparation.
    BeginPrepare { prepare_only: bool },
    /// Start the regular unit.
    StartUnit,
    /// Start the deploy one-shot.
    RunDeployStart,
    /// Check and publish a process-less site.
    CheckAndPublishSite,
    /// Publish or republish the route.
    PublishRoute,
    /// Stop routing before stopping the process.
    UnpublishRoute,
    /// Start periodic health checks.
    StartHealth,
    /// Stop periodic health checks.
    StopHealth,
    /// Stop the managed unit and its cgroup.
    StopUnit,
    /// Reset a failed systemd unit after stopping.
    ResetFailed,
    /// Schedule the next crash retry according to the configured policy.
    ScheduleRetry,
    /// Cancel a scheduled retry.
    CancelRetry,
    /// Record the exit supplied by the effect runner.
    RecordExit,
    /// Adopt the unit discovered at boot.
    AdoptUnit,
    /// Complete the active operation with the resulting status.
    CompleteOperation,
    /// Publish an instance update after state changes.
    PublishState,
}

fn finish(state: &mut SupervisorState, effects: &mut Vec<Effect>) {
    if state.inflight.take().is_some() {
        effects.push(Effect::CompleteOperation);
    }
}

fn begin_start(
    state: &mut SupervisorState,
    validation: StartValidation,
    op: Operation,
    effects: &mut Vec<Effect>,
) {
    match validation {
        StartValidation::Disabled => effects.push(Effect::Reject(RequestError::InstanceDisabled)),
        StartValidation::Invalid(issue) => {
            state.status = InstanceStatus::NeedsConfig;
            state.issue = Some(issue);
            effects.push(Effect::Reject(RequestError::NeedsConfig));
        }
        StartValidation::Ready => {
            if state.retry_pending {
                state.retry_pending = false;
                effects.push(Effect::CancelRetry);
            }
            state.status = InstanceStatus::Preparing;
            state.desired_up = op != Operation::Prepare;
            state.inflight = Some(op);
            state.restart_pending = false;
            state.restarts = 0;
            state.issue = None;
            state.message = None;
            effects.push(Effect::BeginPrepare {
                prepare_only: op == Operation::Prepare,
            });
        }
    }
}

/// Compute a new supervisor state and ordered effects, without doing any I/O.
///
/// The effect runner must discard stale completions from cancelled operations.
pub fn transition(state: &SupervisorState, event: Event) -> (SupervisorState, Vec<Effect>) {
    use InstanceStatus as S;
    let mut next = state.clone();
    let mut effects = Vec::new();
    match event {
        Event::Request {
            operation,
            validation,
        } => {
            match request_policy(next.inflight, operation) {
                RequestPolicy::Join => {
                    effects.push(Effect::Join(operation));
                    return (next, effects);
                }
                RequestPolicy::Busy => {
                    effects.push(Effect::Reject(RequestError::OperationInProgress));
                    return (next, effects);
                }
                RequestPolicy::Preempt => {
                    if let Some(old) = next.inflight.take() {
                        effects.push(Effect::CancelOperation(old));
                    }
                    next.restart_pending = false;
                }
                RequestPolicy::Run => {}
            }
            match operation {
                Operation::Start => match next.status {
                    S::External => effects.push(Effect::Reject(RequestError::ExternallyRunning)),
                    S::Stopped | S::Crashed | S::NeedsConfig => {
                        begin_start(&mut next, validation, operation, &mut effects)
                    }
                    S::Running | S::Unhealthy => effects.push(Effect::CompleteOperation),
                    _ => effects.push(Effect::Reject(RequestError::OperationInProgress)),
                },
                Operation::Prepare => match next.status {
                    S::Stopped | S::Crashed | S::NeedsConfig => {
                        begin_start(&mut next, validation, operation, &mut effects)
                    }
                    S::External => effects.push(Effect::Reject(RequestError::ExternallyRunning)),
                    _ => effects.push(Effect::Reject(RequestError::InvalidState)),
                },
                Operation::Restart => match next.status {
                    S::External => effects.push(Effect::Reject(RequestError::ExternallyRunning)),
                    S::Running | S::Unhealthy => {
                        next.status = S::Stopping;
                        next.desired_up = true;
                        next.inflight = Some(operation);
                        next.restart_pending = true;
                        effects.extend([Effect::UnpublishRoute, Effect::StopHealth, Effect::StopUnit]);
                    }
                    S::Stopped | S::Crashed | S::NeedsConfig => {
                        begin_start(&mut next, validation, operation, &mut effects)
                    }
                    _ => effects.push(Effect::Reject(RequestError::OperationInProgress)),
                },
                Operation::Stop => {
                    next.desired_up = false;
                    if next.retry_pending {
                        next.retry_pending = false;
                        effects.push(Effect::CancelRetry);
                    }
                    match next.status {
                        S::External => effects.push(Effect::Reject(RequestError::NotManaged)),
                        S::Stopped | S::NeedsConfig => effects.push(Effect::CompleteOperation),
                        S::Preparing => {
                            next.status = S::Stopped;
                            next.issue = None;
                            effects.push(Effect::CompleteOperation);
                        }
                        S::Crashed => {
                            next.status = S::Stopped;
                            next.issue = None;
                            effects.push(Effect::CompleteOperation);
                        }
                        S::Starting | S::Running | S::Unhealthy | S::Stopping => {
                            let routed = matches!(next.status, S::Running | S::Unhealthy);
                            next.status = S::Stopping;
                            next.inflight = Some(Operation::Stop);
                            next.restart_pending = false;
                            if routed {
                                effects.extend([Effect::UnpublishRoute, Effect::StopHealth]);
                            }
                            effects.push(Effect::StopUnit);
                        }
                    }
                }
            }
        }
        Event::Prepared(kind) if next.status == S::Preparing => {
            if next.inflight == Some(Operation::Prepare) {
                next.status = S::Stopped;
                next.desired_up = false;
                finish(&mut next, &mut effects);
            } else {
                next.status = S::Starting;
                effects.push(match kind {
                    StartKind::Unit => Effect::StartUnit,
                    StartKind::Deploy => Effect::RunDeployStart,
                    StartKind::Site => Effect::CheckAndPublishSite,
                });
            }
        }
        Event::PreparationFailed(issue) if next.status == S::Preparing => {
            next.status = S::Stopped;
            next.issue = Some(issue);
            next.desired_up = false;
            finish(&mut next, &mut effects);
        }
        Event::PlanInvalid(issue) if matches!(next.status, S::Preparing | S::Starting | S::Stopped) => {
            next.status = S::NeedsConfig;
            next.issue = Some(issue);
            next.desired_up = false;
            finish(&mut next, &mut effects);
        }
        Event::Ready if next.status == S::Starting => {
            next.status = S::Running;
            next.issue = None;
            effects.extend([Effect::PublishRoute, Effect::StartHealth]);
            finish(&mut next, &mut effects);
        }
        Event::Exited { clean, retry, issue }
            if matches!(next.status, S::Starting | S::Running | S::Unhealthy) =>
        {
            let was_ready = matches!(next.status, S::Running | S::Unhealthy);
            if was_ready {
                effects.extend([Effect::UnpublishRoute, Effect::StopHealth]);
            }
            effects.push(Effect::RecordExit);
            if clean && was_ready {
                next.status = S::Stopped;
                next.desired_up = false;
                next.issue = None;
            } else {
                next.status = S::Crashed;
                next.issue = issue;
                if retry && next.desired_up {
                    next.retry_pending = true;
                    effects.push(Effect::ScheduleRetry);
                }
            }
            finish(&mut next, &mut effects);
        }
        Event::PortMismatch if next.status == S::Starting => {
            next.status = S::NeedsConfig;
            next.issue = Some(IssueCode::PortMismatch);
            next.desired_up = false;
            effects.push(Effect::StopUnit);
            finish(&mut next, &mut effects);
        }
        Event::ReadinessTimeout if next.status == S::Starting => {
            next.status = S::Crashed;
            next.issue = Some(IssueCode::ReadinessTimeout);
            effects.push(Effect::StopUnit);
            finish(&mut next, &mut effects);
        }
        Event::DeployFailed(issue) if next.status == S::Starting => {
            next.status = S::Crashed;
            next.issue = Some(issue);
            finish(&mut next, &mut effects);
        }
        Event::HealthFailed if next.status == S::Running => {
            next.status = S::Unhealthy;
        }
        Event::HealthRecovered if next.status == S::Unhealthy => {
            next.status = S::Running;
        }
        Event::UnitInactive if next.status == S::Stopping => {
            effects.push(Effect::ResetFailed);
            if next.restart_pending {
                next.restart_pending = false;
                next.status = S::Preparing;
                effects.push(Effect::BeginPrepare { prepare_only: false });
            } else {
                next.status = S::Stopped;
                finish(&mut next, &mut effects);
            }
        }
        Event::CleanupFailed(issue) if next.status == S::Starting => {
            next.status = S::Stopping;
            next.desired_up = false;
            next.issue = Some(issue);
            finish(&mut next, &mut effects);
        }
        Event::StopIncomplete if next.status == S::Stopping => {
            next.status = S::Running;
            next.desired_up = true;
            next.restart_pending = false;
            finish(&mut next, &mut effects);
        }
        Event::StopTimeout if next.status == S::Stopping => {
            next.restart_pending = false;
            effects.push(Effect::Reject(RequestError::StopTimeout));
            finish(&mut next, &mut effects);
        }
        Event::RetryTimer if next.status == S::Crashed && next.retry_pending && next.desired_up => {
            next.retry_pending = false;
            next.restarts = next.restarts.saturating_add(1);
            next.status = S::Preparing;
            next.inflight = Some(Operation::Start);
            effects.push(Effect::BeginPrepare { prepare_only: false });
        }
        Event::ExternalAppeared if matches!(next.status, S::Stopped | S::Crashed) => {
            if next.retry_pending {
                next.retry_pending = false;
                effects.push(Effect::CancelRetry);
            }
            next.status = S::External;
            next.desired_up = false;
        }
        Event::ExternalDisappeared if next.status == S::External => {
            next.status = S::Stopped;
        }
        Event::ReconcileUnit { ready } if next.status == S::Stopped => {
            next.status = if ready { S::Running } else { S::Starting };
            next.desired_up = true;
            effects.push(Effect::AdoptUnit);
            if ready {
                effects.extend([Effect::PublishRoute, Effect::StartHealth]);
            }
        }
        Event::ReconcileSite if next.status == S::Stopped => {
            next.status = S::Starting;
            next.desired_up = true;
            effects.push(Effect::CheckAndPublishSite);
        }
        Event::ManifestChanged { valid } => match next.status {
            S::Running | S::Unhealthy => next.message = Some("manifest changed; restart to apply".into()),
            S::NeedsConfig if valid => {
                next.status = S::Stopped;
                next.issue = None;
            }
            _ => {}
        },
        Event::RouteFailed if matches!(next.status, S::Running | S::Unhealthy) => {
            next.issue = Some(IssueCode::RouteFailed);
        }
        _ => {}
    }
    if next != *state {
        effects.push(Effect::PublishState);
    }
    (next, effects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use InstanceStatus as S;
    use Operation as O;

    fn request(op: Operation) -> Event {
        Event::Request {
            operation: op,
            validation: StartValidation::Ready,
        }
    }
    fn state(status: InstanceStatus, inflight: Option<Operation>) -> SupervisorState {
        SupervisorState {
            status,
            inflight,
            desired_up: matches!(status, S::Preparing | S::Starting | S::Running | S::Unhealthy),
            ..SupervisorState::default()
        }
    }

    #[test]
    fn d18_preemption_matrix_includes_prepare() {
        let ops = [O::Start, O::Stop, O::Restart, O::Prepare];
        for request in ops {
            assert_eq!(request_policy(None, request), RequestPolicy::Run);
        }
        for active in ops {
            for request in ops {
                let expected = if active == request {
                    RequestPolicy::Join
                } else if request == O::Stop && active != O::Stop {
                    RequestPolicy::Preempt
                } else {
                    RequestPolicy::Busy
                };
                assert_eq!(
                    request_policy(Some(active), request),
                    expected,
                    "{active:?} -> {request:?}"
                );
            }
        }
    }

    #[test]
    fn table_driven_lifecycle_transitions() {
        let cases = vec![
            (
                state(S::Stopped, None),
                request(O::Start),
                S::Preparing,
                Some(Effect::BeginPrepare { prepare_only: false }),
            ),
            (
                state(S::Preparing, Some(O::Start)),
                Event::Prepared(StartKind::Unit),
                S::Starting,
                Some(Effect::StartUnit),
            ),
            (
                state(S::Preparing, Some(O::Start)),
                Event::Prepared(StartKind::Deploy),
                S::Starting,
                Some(Effect::RunDeployStart),
            ),
            (
                state(S::Preparing, Some(O::Start)),
                Event::Prepared(StartKind::Site),
                S::Starting,
                Some(Effect::CheckAndPublishSite),
            ),
            (
                state(S::Preparing, Some(O::Prepare)),
                Event::Prepared(StartKind::Unit),
                S::Stopped,
                Some(Effect::CompleteOperation),
            ),
            (
                state(S::Preparing, Some(O::Start)),
                Event::PreparationFailed(IssueCode::DependencyFailed),
                S::Stopped,
                Some(Effect::CompleteOperation),
            ),
            (
                state(S::Preparing, Some(O::Start)),
                Event::PlanInvalid(IssueCode::ManifestInvalid),
                S::NeedsConfig,
                Some(Effect::CompleteOperation),
            ),
            (
                state(S::Starting, Some(O::Start)),
                Event::Ready,
                S::Running,
                Some(Effect::PublishRoute),
            ),
            (
                state(S::Starting, Some(O::Start)),
                Event::PortMismatch,
                S::NeedsConfig,
                Some(Effect::StopUnit),
            ),
            (
                state(S::Starting, Some(O::Start)),
                Event::ReadinessTimeout,
                S::Crashed,
                Some(Effect::StopUnit),
            ),
            (
                state(S::Starting, Some(O::Start)),
                Event::DeployFailed(IssueCode::Other),
                S::Crashed,
                Some(Effect::CompleteOperation),
            ),
            (state(S::Running, None), Event::HealthFailed, S::Unhealthy, None),
            (
                state(S::Unhealthy, None),
                Event::HealthRecovered,
                S::Running,
                None,
            ),
            (
                state(S::Running, None),
                Event::Exited {
                    clean: true,
                    retry: false,
                    issue: None,
                },
                S::Stopped,
                Some(Effect::UnpublishRoute),
            ),
            (
                state(S::Running, None),
                Event::Exited {
                    clean: false,
                    retry: true,
                    issue: None,
                },
                S::Crashed,
                Some(Effect::ScheduleRetry),
            ),
            (
                state(S::Stopped, None),
                Event::ExternalAppeared,
                S::External,
                None,
            ),
            (
                state(S::External, None),
                Event::ExternalDisappeared,
                S::Stopped,
                None,
            ),
            (
                state(S::Stopped, None),
                Event::ReconcileUnit { ready: true },
                S::Running,
                Some(Effect::AdoptUnit),
            ),
            (
                state(S::Stopped, None),
                Event::ReconcileUnit { ready: false },
                S::Starting,
                Some(Effect::AdoptUnit),
            ),
            (
                state(S::Stopped, None),
                Event::ReconcileSite,
                S::Starting,
                Some(Effect::CheckAndPublishSite),
            ),
            (
                state(S::NeedsConfig, None),
                Event::ManifestChanged { valid: true },
                S::Stopped,
                None,
            ),
        ];
        for (start, event, expected, effect) in cases {
            let (end, effects) = transition(&start, event.clone());
            assert_eq!(end.status, expected, "{start:?}, {event:?}");
            if let Some(effect) = effect {
                assert!(effects.contains(&effect), "{start:?}, {event:?}: {effects:?}");
            }
        }
    }

    #[test]
    fn stop_preempts_and_does_not_restart() {
        for op in [O::Start, O::Restart, O::Prepare] {
            let start = state(S::Preparing, Some(op));
            let (end, effects) = transition(&start, request(O::Stop));
            assert_eq!(end.status, S::Stopped);
            assert!(!end.desired_up);
            assert!(effects.contains(&Effect::CancelOperation(op)));
        }
        let start = state(S::Starting, Some(O::Start));
        let (end, effects) = transition(&start, request(O::Stop));
        assert_eq!(end.status, S::Stopping);
        assert_eq!(end.inflight, Some(O::Stop));
        assert!(effects.contains(&Effect::CancelOperation(O::Start)));
        assert!(effects.contains(&Effect::StopUnit));
        let (end, _) = transition(&end, Event::UnitInactive);
        assert_eq!(end.status, S::Stopped);
        assert!(!end.restart_pending);
    }

    #[test]
    fn stop_timeout_stays_stopping_and_next_stop_reissues() {
        let start = state(S::Stopping, Some(O::Stop));
        let (timed_out, effects) = transition(&start, Event::StopTimeout);
        assert_eq!(timed_out.status, S::Stopping);
        assert_eq!(timed_out.inflight, None);
        assert!(effects.contains(&Effect::Reject(RequestError::StopTimeout)));
        let (_, effects) = transition(&timed_out, request(O::Stop));
        assert!(effects.contains(&Effect::StopUnit));
    }

    #[test]
    fn restart_and_retry_flow_and_external_guards() {
        let running = state(S::Running, None);
        let (stopping, effects) = transition(&running, request(O::Restart));
        assert_eq!(stopping.status, S::Stopping);
        assert!(
            effects.iter().position(|e| *e == Effect::UnpublishRoute)
                < effects.iter().position(|e| *e == Effect::StopUnit)
        );
        let (preparing, _) = transition(&stopping, Event::UnitInactive);
        assert_eq!(preparing.status, S::Preparing);
        assert_eq!(preparing.inflight, Some(O::Restart));
        let (crashed, _) = transition(
            &running,
            Event::Exited {
                clean: false,
                retry: true,
                issue: None,
            },
        );
        let (retrying, _) = transition(&crashed, Event::RetryTimer);
        assert_eq!(retrying.inflight, Some(O::Start));
        assert_eq!(retrying.restarts, 1);
        let (_, effects) = transition(&retrying, request(O::Start));
        assert_eq!(effects, vec![Effect::Join(O::Start)]);
        let external = state(S::External, None);
        let (_, effects) = transition(&external, request(O::Stop));
        assert!(effects.contains(&Effect::Reject(RequestError::NotManaged)));
        let (_, effects) = transition(&external, request(O::Start));
        assert!(effects.contains(&Effect::Reject(RequestError::ExternallyRunning)));
    }
}
