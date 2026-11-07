//! Recheckable installation steps.

/// The observed state of an installation step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    /// The required state already holds.
    Done(String),
    /// The step needs to be applied.
    Pending(String),
    /// The step is not applicable or cannot yet run.
    Skipped(String),
    /// The check failed and must not be treated as complete.
    Failed(String),
}
/// The result of applying one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    /// Stable step identifier.
    pub id: &'static str,
    /// Result of this invocation.
    pub state: OutcomeState,
    /// Human-readable result.
    pub detail: String,
}
/// Possible installation results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeState {
    /// Nothing needed changing.
    AlreadyDone,
    /// The step changed state.
    Changed,
    /// The step was inapplicable.
    Skipped,
    /// An operation failed.
    Failed,
}
impl StepOutcome {
    /// Construct an outcome for an individual step.
    pub fn new(id: &'static str, state: OutcomeState, detail: impl Into<String>) -> Self {
        Self {
            id,
            state,
            detail: detail.into(),
        }
    }
}
/// A recheckable, synchronous installation action.
pub trait Step {
    /// Stable identifier.
    fn id(&self) -> &'static str;
    /// Description of the operation.
    fn description(&self) -> &'static str;
    /// Whether applying the step needs root.
    fn needs_root(&self) -> bool;
    /// A user-facing command to fix this step.
    fn fix_command(&self) -> &'static str;
    /// Inspect current state without changes.
    fn check(&self) -> StepState;
    /// Apply this step, including a fresh check.
    fn apply(&self) -> StepOutcome;
}
/// Turn a check result into an outcome when no action should run.
pub(crate) fn preflight(id: &'static str, state: StepState) -> Result<(), StepOutcome> {
    match state {
        StepState::Done(s) => Err(StepOutcome::new(id, OutcomeState::AlreadyDone, s)),
        StepState::Skipped(s) => Err(StepOutcome::new(id, OutcomeState::Skipped, s)),
        StepState::Failed(s) => Err(StepOutcome::new(id, OutcomeState::Failed, s)),
        StepState::Pending(_) => Ok(()),
    }
}
