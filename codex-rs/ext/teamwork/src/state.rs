//! Deterministic `/teamwork` run state machine.
//!
//! The model drives a run through tools, but every phase transition, validation rule, gate
//! requirement, and retry budget lives here so the harness, not the prompt, decides when a run may
//! advance or finish.

use serde::Deserialize;
use serde::Serialize;
use std::fmt;

/// Fix rounds allowed per milestone (and for the final audit) before the run blocks for the user.
pub const MAX_FIX_ROUNDS: u32 = 3;
/// Consecutive automatic continuations without any state change before the run blocks.
pub const MAX_IDLE_CONTINUATIONS_WITHOUT_PROGRESS: u32 = 3;
/// Milestone id used when submitting fixes for a failed final audit.
pub const FINAL_MILESTONE_ID: &str = "final";

/// Gates that each milestone must pass before the run may advance.
pub const MILESTONE_GATES: [GateKind; 3] =
    [GateKind::Critic, GateKind::Challenger, GateKind::Auditor];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The lead is clarifying the goal and has not recorded a brief yet.
    Scoping,
    /// A brief exists; the lead must record a milestone plan.
    Planning,
    /// Workers are implementing the current milestone.
    Executing,
    /// Independent gates are reviewing the current milestone.
    Gating,
    /// The final success auditor is reviewing the whole result.
    FinalAudit,
    /// The final audit failed; the lead is fixing its findings.
    FinalFix,
    /// Every milestone and the final audit passed.
    Complete,
    /// The run is waiting for the user.
    Blocked,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Scoping => "scoping",
            Phase::Planning => "planning",
            Phase::Executing => "executing",
            Phase::Gating => "gating",
            Phase::FinalAudit => "final_audit",
            Phase::FinalFix => "final_fix",
            Phase::Complete => "complete",
            Phase::Blocked => "blocked",
        }
    }

    /// Whether the harness should keep the lead working when its thread goes idle.
    pub fn wants_continuation(self) -> bool {
        matches!(
            self,
            Phase::Planning
                | Phase::Executing
                | Phase::Gating
                | Phase::FinalAudit
                | Phase::FinalFix
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Complete)
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Critic,
    Challenger,
    Auditor,
    SuccessAuditor,
}

impl GateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GateKind::Critic => "critic",
            GateKind::Challenger => "challenger",
            GateKind::Auditor => "auditor",
            GateKind::SuccessAuditor => "success_auditor",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            GateKind::Critic => "Critic",
            GateKind::Challenger => "Challenger",
            GateKind::Auditor => "Auditor",
            GateKind::SuccessAuditor => "Success Auditor",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    Open,
    Passed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneStatus {
    Pending,
    InProgress,
    InReview,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    /// How the criterion is verified: a command, test, or concrete check.
    pub verification: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub objective: String,
    #[serde(default)]
    pub requirements: Vec<String>,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    #[serde(default)]
    pub assumptions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub description: String,
    /// Files or directories this track exclusively owns. Directories end with `/`.
    pub files: Vec<String>,
    /// Role of the agent that implements the track, for example `worker`.
    #[serde(default = "default_track_role")]
    pub role: String,
    /// Model the lead chose for this track. `None` inherits the lead's model.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort the lead chose for this track. `None` inherits the lead's effort.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

fn default_track_role() -> String {
    "worker".to_string()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestonePlan {
    pub id: String,
    pub title: String,
    pub deliverable: String,
    pub tracks: Vec<Track>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub id: String,
    pub title: String,
    pub deliverable: String,
    pub tracks: Vec<Track>,
    pub status: MilestoneStatus,
    /// Failed review rounds since the budget was last reset.
    pub fix_rounds: u32,
    /// Monotonic review counter used to group gate tickets.
    pub review_round: u32,
    /// Lead-provided summary from the latest submission for review.
    #[serde(default)]
    pub last_submission: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateTicket {
    pub token: String,
    /// `None` for the final success audit.
    pub milestone_id: Option<String>,
    pub kind: GateKind,
    pub round: u32,
    pub status: TicketStatus,
    #[serde(default)]
    pub reviewer_thread_id: Option<String>,
    #[serde(default)]
    pub findings: String,
    #[serde(default)]
    pub evidence: String,
}

/// What changed after a gate verdict was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerdictOutcome {
    /// Other gates for the same review round are still open.
    AwaitingOtherGates,
    /// The milestone passed every gate; the next milestone is now executing.
    MilestonePassed { next_milestone_id: String },
    /// The last milestone passed; the final success audit ticket is open.
    FinalAuditOpened { ticket: GateTicket },
    /// At least one gate failed; the milestone returns to execution for fixes.
    MilestoneNeedsFixes { fix_round: u32 },
    /// The final audit failed; the lead must fix and resubmit.
    FinalAuditFailed { fix_round: u32 },
    /// The fix budget is exhausted; the run is blocked for the user.
    Blocked { reason: String },
    /// The final audit passed; the run is complete.
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamworkError(pub String);

impl fmt::Display for TeamworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TeamworkError {}

fn err<T>(message: impl Into<String>) -> Result<T, TeamworkError> {
    Err(TeamworkError(message.into()))
}

/// Result of checking whether an idle lead should be continued automatically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContinuationDecision {
    /// The run does not need a continuation in its current phase.
    Skip,
    /// Start a continuation turn.
    Continue,
    /// The run made no progress for too long and is now blocked.
    BlockedNoProgress { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamworkRun {
    pub run_id: String,
    pub lead_thread_id: String,
    pub goal: String,
    pub phase: Phase,
    #[serde(default)]
    pub resume_phase: Option<Phase>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    #[serde(default)]
    pub brief: Option<Brief>,
    #[serde(default)]
    pub milestones: Vec<Milestone>,
    #[serde(default)]
    pub current_milestone: usize,
    #[serde(default)]
    pub gates: Vec<GateTicket>,
    #[serde(default)]
    pub final_fix_rounds: u32,
    #[serde(default)]
    pub final_review_round: u32,
    #[serde(default)]
    pub final_submission: Option<String>,
    /// Incremented on every accepted state change.
    pub revision: u64,
    #[serde(default)]
    pub idle_without_progress: u32,
    #[serde(default)]
    pub last_idle_revision: Option<u64>,
    #[serde(default)]
    pub next_token_seq: u64,
}

impl TeamworkRun {
    pub fn new(
        run_id: impl Into<String>,
        lead_thread_id: impl Into<String>,
        goal: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            lead_thread_id: lead_thread_id.into(),
            goal: goal.into(),
            phase: Phase::Scoping,
            resume_phase: None,
            blocked_reason: None,
            brief: None,
            milestones: Vec::new(),
            current_milestone: 0,
            gates: Vec::new(),
            final_fix_rounds: 0,
            final_review_round: 0,
            final_submission: None,
            revision: 0,
            idle_without_progress: 0,
            last_idle_revision: None,
            next_token_seq: 0,
        }
    }

    pub fn current(&self) -> Option<&Milestone> {
        self.milestones.get(self.current_milestone)
    }

    pub fn open_tickets(&self) -> impl Iterator<Item = &GateTicket> {
        self.gates
            .iter()
            .filter(|ticket| ticket.status == TicketStatus::Open)
    }

    pub fn ticket(&self, token: &str) -> Option<&GateTicket> {
        self.gates.iter().find(|ticket| ticket.token == token)
    }

    fn bump(&mut self) {
        self.revision += 1;
        self.idle_without_progress = 0;
    }

    fn require_phase(&self, allowed: &[Phase], action: &str) -> Result<(), TeamworkError> {
        if allowed.contains(&self.phase) {
            return Ok(());
        }
        let allowed = allowed
            .iter()
            .map(|phase| phase.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        err(format!(
            "cannot {action} while the run is in phase `{}`; allowed phases: {allowed}",
            self.phase
        ))
    }

    /// Records or replaces the scoping brief and moves the run to planning.
    pub fn record_brief(&mut self, brief: Brief) -> Result<(), TeamworkError> {
        self.require_phase(&[Phase::Scoping, Phase::Planning], "record the brief")?;
        if brief.objective.trim().is_empty() {
            return err("the brief needs a non-empty objective");
        }
        if brief.acceptance_criteria.is_empty() {
            return err("the brief needs at least one acceptance criterion");
        }
        let mut ids = Vec::new();
        for criterion in &brief.acceptance_criteria {
            let id = criterion.id.trim();
            if id.is_empty() || criterion.description.trim().is_empty() {
                return err("every acceptance criterion needs an id and a description");
            }
            if criterion.verification.trim().is_empty() {
                return err(format!(
                    "acceptance criterion `{id}` needs a verification method (a command, test, or concrete check)"
                ));
            }
            if ids.contains(&id) {
                return err(format!("duplicate acceptance criterion id `{id}`"));
            }
            ids.push(id);
        }
        self.brief = Some(brief);
        self.phase = Phase::Planning;
        self.bump();
        Ok(())
    }

    /// Records the milestone plan and starts executing the first milestone.
    pub fn record_plan(&mut self, plan: Vec<MilestonePlan>) -> Result<(), TeamworkError> {
        self.require_phase(&[Phase::Planning], "record the plan")?;
        if plan.is_empty() {
            return err("the plan needs at least one milestone");
        }
        let mut milestone_ids: Vec<&str> = Vec::new();
        for milestone in &plan {
            let id = milestone.id.trim();
            if id.is_empty() || milestone.title.trim().is_empty() {
                return err("every milestone needs an id and a title");
            }
            if id == FINAL_MILESTONE_ID {
                return err(format!("milestone id `{FINAL_MILESTONE_ID}` is reserved"));
            }
            if milestone_ids.contains(&id) {
                return err(format!("duplicate milestone id `{id}`"));
            }
            milestone_ids.push(id);
            if milestone.deliverable.trim().is_empty() {
                return err(format!("milestone `{id}` needs a concrete deliverable"));
            }
            validate_tracks(id, &milestone.tracks)?;
        }
        self.milestones = plan
            .into_iter()
            .map(|milestone| Milestone {
                id: milestone.id.trim().to_string(),
                title: milestone.title,
                deliverable: milestone.deliverable,
                tracks: milestone.tracks,
                status: MilestoneStatus::Pending,
                fix_rounds: 0,
                review_round: 0,
                last_submission: None,
            })
            .collect();
        self.current_milestone = 0;
        self.milestones[0].status = MilestoneStatus::InProgress;
        self.phase = Phase::Executing;
        self.bump();
        Ok(())
    }

    /// Submits the current milestone (or the final fixes) for independent review and opens one
    /// gate ticket per required gate.
    pub fn submit_for_review(
        &mut self,
        milestone_id: &str,
        summary: String,
    ) -> Result<Vec<GateTicket>, TeamworkError> {
        self.require_phase(
            &[Phase::Executing, Phase::FinalFix],
            "submit work for review",
        )?;
        if summary.trim().is_empty() {
            return err("the submission needs a summary of what changed and how it was verified");
        }
        if self.phase == Phase::FinalFix {
            if milestone_id != FINAL_MILESTONE_ID {
                return err(format!(
                    "the final audit failed; submit fixes with milestone_id `{FINAL_MILESTONE_ID}`"
                ));
            }
            self.final_submission = Some(summary);
            let ticket = self.open_final_audit_ticket();
            self.phase = Phase::FinalAudit;
            self.bump();
            return Ok(vec![ticket]);
        }

        let Some(current) = self.milestones.get(self.current_milestone) else {
            return err("there is no milestone to submit");
        };
        if current.id != milestone_id {
            return err(format!(
                "milestone `{milestone_id}` is not the current milestone; the current milestone is `{}`",
                current.id
            ));
        }
        let index = self.current_milestone;
        self.milestones[index].review_round += 1;
        self.milestones[index].status = MilestoneStatus::InReview;
        self.milestones[index].last_submission = Some(summary);
        let round = self.milestones[index].review_round;
        let milestone_id = self.milestones[index].id.clone();
        let tickets = MILESTONE_GATES
            .iter()
            .map(|kind| self.open_ticket(Some(milestone_id.clone()), *kind, round))
            .collect();
        self.phase = Phase::Gating;
        self.bump();
        Ok(tickets)
    }

    fn open_final_audit_ticket(&mut self) -> GateTicket {
        self.final_review_round += 1;
        let round = self.final_review_round;
        self.open_ticket(None, GateKind::SuccessAuditor, round)
    }

    fn open_ticket(
        &mut self,
        milestone_id: Option<String>,
        kind: GateKind,
        round: u32,
    ) -> GateTicket {
        self.next_token_seq += 1;
        let scope = milestone_id.as_deref().unwrap_or(FINAL_MILESTONE_ID);
        let token = format!(
            "tw-{}-{scope}-r{round}-{}-{}",
            short_id(&self.run_id),
            kind.as_str(),
            self.next_token_seq
        );
        let ticket = GateTicket {
            token,
            milestone_id,
            kind,
            round,
            status: TicketStatus::Open,
            reviewer_thread_id: None,
            findings: String::new(),
            evidence: String::new(),
        };
        self.gates.push(ticket.clone());
        ticket
    }

    /// Records one gate verdict submitted by an independent reviewer thread.
    ///
    /// `reviewer_modified_files` is true when the reviewer thread itself edited files during this
    /// run; such a thread is an implementer, not an independent gate.
    pub fn record_verdict(
        &mut self,
        token: &str,
        reviewer_thread_id: &str,
        reviewer_modified_files: bool,
        verdict: Verdict,
        findings: String,
        evidence: String,
    ) -> Result<VerdictOutcome, TeamworkError> {
        let Some(index) = self.gates.iter().position(|ticket| ticket.token == token) else {
            return err(format!("unknown gate token `{token}`"));
        };
        if self.gates[index].status != TicketStatus::Open {
            return err(format!("gate token `{token}` was already used"));
        }
        if reviewer_thread_id == self.lead_thread_id {
            return err(
                "the lead agent cannot submit gate verdicts; spawn an independent reviewer",
            );
        }
        if reviewer_modified_files {
            return err(
                "this agent edited files during the run, so it cannot act as an independent gate",
            );
        }
        let milestone_id = self.gates[index].milestone_id.clone();
        let round = self.gates[index].round;
        let reused_reviewer = self.gates.iter().any(|ticket| {
            ticket.milestone_id == milestone_id
                && ticket.round == round
                && ticket.reviewer_thread_id.as_deref() == Some(reviewer_thread_id)
        });
        if reused_reviewer {
            return err(
                "this agent already reviewed this round as another gate; each gate needs its own agent",
            );
        }
        if verdict == Verdict::Fail && findings.trim().is_empty() {
            return err("a failing verdict needs concrete findings");
        }
        if verdict == Verdict::Pass && evidence.trim().is_empty() {
            return err(
                "a passing verdict needs evidence: the checks you ran and what they showed",
            );
        }

        let ticket = &mut self.gates[index];
        ticket.status = match verdict {
            Verdict::Pass => TicketStatus::Passed,
            Verdict::Fail => TicketStatus::Failed,
        };
        ticket.reviewer_thread_id = Some(reviewer_thread_id.to_string());
        ticket.findings = findings;
        ticket.evidence = evidence;
        self.bump();

        // A lead may block the run while reviewers are still working. Their verdicts still count,
        // but the run stays blocked until the user responds.
        let was_blocked = self.phase == Phase::Blocked;
        let outcome = match milestone_id {
            Some(milestone_id) => self.evaluate_milestone_round(&milestone_id, round),
            None => self.evaluate_final_audit(verdict),
        };
        if was_blocked && self.phase != Phase::Blocked {
            self.resume_phase = Some(self.phase);
            self.phase = Phase::Blocked;
        }
        Ok(outcome)
    }

    fn evaluate_milestone_round(&mut self, milestone_id: &str, round: u32) -> VerdictOutcome {
        let round_tickets = self
            .gates
            .iter()
            .filter(|ticket| {
                ticket.milestone_id.as_deref() == Some(milestone_id) && ticket.round == round
            })
            .collect::<Vec<_>>();
        if round_tickets
            .iter()
            .any(|ticket| ticket.status == TicketStatus::Open)
        {
            return VerdictOutcome::AwaitingOtherGates;
        }
        let failed = round_tickets
            .iter()
            .any(|ticket| ticket.status == TicketStatus::Failed);
        let Some(index) = self
            .milestones
            .iter()
            .position(|milestone| milestone.id == milestone_id)
        else {
            return VerdictOutcome::AwaitingOtherGates;
        };

        if failed {
            self.milestones[index].fix_rounds += 1;
            let fix_round = self.milestones[index].fix_rounds;
            if fix_round > MAX_FIX_ROUNDS {
                let reason = format!(
                    "milestone `{milestone_id}` failed its gates {fix_round} times; the fix budget of {MAX_FIX_ROUNDS} rounds is exhausted"
                );
                self.milestones[index].status = MilestoneStatus::InProgress;
                self.block_with_resume(reason.clone(), Phase::Executing);
                return VerdictOutcome::Blocked { reason };
            }
            self.milestones[index].status = MilestoneStatus::InProgress;
            self.phase = Phase::Executing;
            return VerdictOutcome::MilestoneNeedsFixes { fix_round };
        }

        self.milestones[index].status = MilestoneStatus::Done;
        if index + 1 < self.milestones.len() {
            self.current_milestone = index + 1;
            self.milestones[index + 1].status = MilestoneStatus::InProgress;
            self.phase = Phase::Executing;
            return VerdictOutcome::MilestonePassed {
                next_milestone_id: self.milestones[index + 1].id.clone(),
            };
        }
        let ticket = self.open_final_audit_ticket();
        self.phase = Phase::FinalAudit;
        VerdictOutcome::FinalAuditOpened { ticket }
    }

    fn evaluate_final_audit(&mut self, verdict: Verdict) -> VerdictOutcome {
        match verdict {
            Verdict::Pass => {
                self.phase = Phase::Complete;
                VerdictOutcome::Complete
            }
            Verdict::Fail => {
                self.final_fix_rounds += 1;
                let fix_round = self.final_fix_rounds;
                if fix_round > MAX_FIX_ROUNDS {
                    let reason = format!(
                        "the final success audit failed {fix_round} times; the fix budget of {MAX_FIX_ROUNDS} rounds is exhausted"
                    );
                    self.block_with_resume(reason.clone(), Phase::FinalFix);
                    return VerdictOutcome::Blocked { reason };
                }
                self.phase = Phase::FinalFix;
                VerdictOutcome::FinalAuditFailed { fix_round }
            }
        }
    }

    fn block_with_resume(&mut self, reason: String, resume_phase: Phase) {
        self.resume_phase = Some(resume_phase);
        self.blocked_reason = Some(reason);
        self.phase = Phase::Blocked;
        self.bump();
    }

    /// Blocks the run because the lead needs a decision only the user can make.
    pub fn block(&mut self, reason: String) -> Result<(), TeamworkError> {
        if reason.trim().is_empty() {
            return err("blocking the run needs a reason the user can act on");
        }
        if matches!(self.phase, Phase::Complete | Phase::Blocked) {
            return err(format!("cannot block a run in phase `{}`", self.phase));
        }
        let resume_phase = self.phase;
        self.block_with_resume(reason, resume_phase);
        Ok(())
    }

    /// Resumes a blocked run after the user responds. Exhausted fix budgets are renewed so the
    /// user's guidance gets a fresh set of rounds.
    pub fn resume_after_user_input(&mut self) -> bool {
        if self.phase != Phase::Blocked {
            return false;
        }
        let resume_phase = self.resume_phase.take().unwrap_or(Phase::Executing);
        if let Some(milestone) = self.milestones.get_mut(self.current_milestone)
            && milestone.fix_rounds > MAX_FIX_ROUNDS
        {
            milestone.fix_rounds = 0;
        }
        if self.final_fix_rounds > MAX_FIX_ROUNDS {
            self.final_fix_rounds = 0;
        }
        // Reopen review if the run was blocked while gates were outstanding.
        self.phase = resume_phase;
        self.blocked_reason = None;
        self.last_idle_revision = None;
        self.bump();
        true
    }

    /// Decides whether an idle lead thread should be continued and tracks no-progress loops.
    pub fn on_idle(&mut self) -> ContinuationDecision {
        if !self.phase.wants_continuation() {
            return ContinuationDecision::Skip;
        }
        if self.last_idle_revision == Some(self.revision) {
            self.idle_without_progress += 1;
        } else {
            self.idle_without_progress = 0;
            self.last_idle_revision = Some(self.revision);
        }
        if self.idle_without_progress >= MAX_IDLE_CONTINUATIONS_WITHOUT_PROGRESS {
            let reason = format!(
                "no recorded progress after {MAX_IDLE_CONTINUATIONS_WITHOUT_PROGRESS} automatic continuations in phase `{}`",
                self.phase
            );
            let resume_phase = self.phase;
            self.block_with_resume(reason.clone(), resume_phase);
            return ContinuationDecision::BlockedNoProgress { reason };
        }
        ContinuationDecision::Continue
    }
}

fn short_id(run_id: &str) -> &str {
    let end = run_id
        .char_indices()
        .nth(8)
        .map_or(run_id.len(), |(index, _)| index);
    &run_id[..end]
}

fn validate_tracks(milestone_id: &str, tracks: &[Track]) -> Result<(), TeamworkError> {
    if tracks.is_empty() {
        return err(format!(
            "milestone `{milestone_id}` needs at least one track"
        ));
    }
    let mut track_ids: Vec<&str> = Vec::new();
    for track in tracks {
        let id = track.id.trim();
        if id.is_empty() || track.description.trim().is_empty() {
            return err(format!(
                "every track in milestone `{milestone_id}` needs an id and a description"
            ));
        }
        if track_ids.contains(&id) {
            return err(format!(
                "duplicate track id `{id}` in milestone `{milestone_id}`"
            ));
        }
        track_ids.push(id);
        if track
            .files
            .iter()
            .all(|file| normalize_path(file).is_empty())
        {
            return err(format!(
                "track `{id}` in milestone `{milestone_id}` must list the files or directories it owns"
            ));
        }
    }
    for (left_index, left) in tracks.iter().enumerate() {
        for right in &tracks[left_index + 1..] {
            for left_file in &left.files {
                for right_file in &right.files {
                    if paths_overlap(left_file, right_file) {
                        return err(format!(
                            "tracks `{}` and `{}` in milestone `{milestone_id}` both claim `{}` / `{}`; parallel tracks must own non-overlapping files",
                            left.id.trim(),
                            right.id.trim(),
                            left_file.trim(),
                            right_file.trim()
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn normalize_path(path: &str) -> String {
    let mut path = path.trim().replace('\\', "/");
    while let Some(stripped) = path.strip_prefix("./") {
        path = stripped.to_string();
    }
    while path.ends_with('/') {
        path.pop();
    }
    path
}

/// Whether two owned paths overlap: equal, or one is a directory containing the other.
pub fn paths_overlap(left: &str, right: &str) -> bool {
    let left = normalize_path(left);
    let right = normalize_path(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == "." || right == "." {
        return true;
    }
    left == right
        || right.starts_with(&format!("{left}/"))
        || left.starts_with(&format!("{right}/"))
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
